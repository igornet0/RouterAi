//! Agent tool-calling loop: model → tool_calls → tools → tool results → model → final.
//!
//! Mock HTTP providers stand in for the model; the mock answers from what it
//! received, so assertions prove the tool results actually reached the next turn.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use routerai::{
    Agent, Event, Handler, Permission, Permissions, RouterError, RouterResult, RouterRuntime,
    RunStatus, RunStepKind, ToolDefinition,
};
use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::http::HttpClient;
use universal_ai::{AiClient, Anthropic, Gemini, ModelPricing, OpenAICompatible, ProviderId};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ---------- test tools ----------

type ToolFn = Arc<dyn Fn(&Value) -> RouterResult<Value> + Send + Sync>;

/// Tool handler backed by a closure; counts invocations and records inputs.
#[derive(Clone)]
struct TestTool {
    calls: Arc<AtomicUsize>,
    inputs: Arc<Mutex<Vec<Value>>>,
    run: ToolFn,
}

impl TestTool {
    fn new(run: impl Fn(&Value) -> RouterResult<Value> + Send + Sync + 'static) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            inputs: Arc::new(Mutex::new(Vec::new())),
            run: Arc::new(run),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl routerai::tool::ToolHandler for TestTool {
    async fn call(&self, input: Value) -> RouterResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(input.clone());
        (self.run)(&input)
    }
}

struct Tools {
    add: TestTool,
    lookup: TestTool,
    fails: TestTool,
    spy: TestTool,
}

async fn register_tools(rt: &RouterRuntime) -> Tools {
    let tools = Tools {
        add: TestTool::new(|v| {
            let a = v["a"]
                .as_i64()
                .ok_or(RouterError::Tool("a required".into()))?;
            let b = v["b"]
                .as_i64()
                .ok_or(RouterError::Tool("b required".into()))?;
            Ok(json!(a + b))
        }),
        lookup: TestTool::new(|v| Ok(json!({ "key": v["key"], "value": "42 EUR" }))),
        fails: TestTool::new(|_| Err(RouterError::Tool("boom: upstream unavailable".into()))),
        spy: TestTool::new(|_| Ok(json!("spy executed"))),
    };
    let reg = rt.tools().registry();
    let def = |id: &str, desc: &str, schema: Value| ToolDefinition {
        id: id.into(),
        name: id.into(),
        description: desc.into(),
        input_schema: schema,
        output_schema: None,
        permissions: Permissions::none(),
    };
    reg.register(
        def(
            "math.add",
            "Add two integers",
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"]
            }),
        ),
        Arc::new(tools.add.clone()),
    )
    .await
    .unwrap();
    reg.register(
        def(
            "kv.lookup",
            "Look up a value",
            json!({ "type": "object", "properties": { "key": { "type": "string" } } }),
        ),
        Arc::new(tools.lookup.clone()),
    )
    .await
    .unwrap();
    reg.register(
        def("always.fails", "Fails", json!({ "type": "object" })),
        Arc::new(tools.fails.clone()),
    )
    .await
    .unwrap();
    reg.register(
        def(
            "spy.tool",
            "Must never run unless the model calls it",
            json!({ "type": "object" }),
        ),
        Arc::new(tools.spy.clone()),
    )
    .await
    .unwrap();
    tools
}

/// The runtime refuses models without a known price: register one.
fn priced(ai: AiClient, provider: ProviderId, model: &str) -> AiClient {
    ai.pricing().upsert(ModelPricing::per_million(
        provider,
        model,
        "1".parse().unwrap(),
        "2".parse().unwrap(),
    ));
    ai
}

fn agent_with_tools(tools: &[&str]) -> Agent {
    let mut agent = routerai::published_agent("tools-agent", "Use tools when helpful.");
    agent.tools = tools.iter().map(|t| t.to_string()).collect();
    agent
}

// ---------- OpenAI-compatible mock model ----------

fn body_of(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

/// `(tool_call_id, content)` of tool messages in an OpenAI-style request.
fn tool_messages(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn tool_call_reply(calls: &[(&str, &str, &str)]) -> ResponseTemplate {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(id, name, args)| {
            json!({ "id": id, "type": "function",
                    "function": { "name": name, "arguments": args } })
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": null, "tool_calls": calls },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    }))
}

fn final_reply(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 6, "total_tokens": 18 }
    }))
}

async fn openai_runtime(
    model: impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
) -> (MockServer, RouterRuntime, Tools) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(model)
        .mount(&server)
        .await;
    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("sk-test".into()))
        .build()
        .unwrap();
    // The agents' default model is served by the only configured provider.
    let ai = priced(
        AiClient::builder().provider(provider).build().unwrap(),
        ProviderId::openai_compatible(),
        "deepseek-chat",
    );
    let rt = RouterRuntime::builder()
        .ai(Arc::new(ai))
        .build()
        .await
        .unwrap();
    let tools = register_tools(&rt).await;
    (server, rt, tools)
}

async fn model_requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(body_of)
        .collect()
}

fn offered_tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

async fn run(rt: &RouterRuntime, agent: Agent, message: &str) -> routerai::AgentRun {
    let id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.start_run(&id, json!({ "message": message }), None)
        .await
        .unwrap()
}

// ---------- A. model → tool_call → tool → model → final ----------

#[tokio::test]
async fn a_tool_result_feeds_next_model_turn() {
    let (server, rt, tools) = openai_runtime(|req| {
        let body = body_of(req);
        match tool_messages(&body).first() {
            None => tool_call_reply(&[("call_add_1", "math_add", r#"{"a":2,"b":3}"#)]),
            Some((_, result)) => final_reply(&format!("The sum is {result}.")),
        }
    })
    .await;

    let run = run(&rt, agent_with_tools(&["math.add"]), "What is 2+3?").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    let out = run.output.as_ref().unwrap();
    assert_eq!(out["text"], "The sum is 5.");
    assert_eq!(out["turns"], 2);
    assert_eq!(out["tool_calls"], 1);
    assert_eq!(tools.add.count(), 1);
    // Usage accumulates across both turns.
    assert_eq!(run.usage.total_tokens, 15 + 18);

    let reqs = model_requests(&server).await;
    assert_eq!(reqs.len(), 2, "two model turns");
    // Turn 2 carries the assistant tool-call message and the matching tool result.
    let msgs = reqs[1]["messages"].as_array().unwrap();
    let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "call_add_1");
    assert_eq!(
        tool_messages(&reqs[1]),
        vec![("call_add_1".to_string(), "5".to_string())]
    );

    let kinds: Vec<_> = run.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RunStepKind::Event,
            RunStepKind::Llm,
            RunStepKind::Tool,
            RunStepKind::Observe,
            RunStepKind::Llm,
            RunStepKind::Result,
        ]
    );
}

// ---------- B. several tool calls in one response ----------

#[tokio::test]
async fn b_multiple_tool_calls_in_one_response() {
    let (server, rt, tools) = openai_runtime(|req| {
        let results = tool_messages(&body_of(req));
        if results.is_empty() {
            tool_call_reply(&[
                ("call_a", "math_add", r#"{"a":40,"b":2}"#),
                ("call_b", "kv_lookup", r#"{"key":"price"}"#),
            ])
        } else {
            let joined: Vec<String> = results.iter().map(|(id, c)| format!("{id}={c}")).collect();
            final_reply(&joined.join("; "))
        }
    })
    .await;

    let run = run(&rt, agent_with_tools(&["math.add", "kv.lookup"]), "go").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(tools.add.count(), 1);
    assert_eq!(tools.lookup.count(), 1);
    let reqs = model_requests(&server).await;
    assert_eq!(reqs.len(), 2);
    let results = tool_messages(&reqs[1]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], ("call_a".to_string(), "42".to_string()));
    assert_eq!(results[1].0, "call_b");
    assert_eq!(
        serde_json::from_str::<Value>(&results[1].1).unwrap(),
        json!({ "key": "price", "value": "42 EUR" })
    );
    assert_eq!(run.output.as_ref().unwrap()["tool_calls"], 2);
}

// ---------- C. unknown / not-allowed tool ----------

#[tokio::test]
async fn c_unknown_or_disallowed_tool_is_not_executed() {
    let (server, rt, tools) = openai_runtime(|req| {
        let results = tool_messages(&body_of(req));
        if results.is_empty() {
            tool_call_reply(&[
                // Registered but not on this agent's allow-list.
                ("call_spy", "spy_tool", "{}"),
                // Not registered at all.
                ("call_ghost", "delete_everything", "{}"),
            ])
        } else {
            final_reply("ok without those tools")
        }
    })
    .await;

    let run = run(&rt, agent_with_tools(&["math.add"]), "try it").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(tools.spy.count(), 0, "disallowed tool must not execute");
    let reqs = model_requests(&server).await;
    assert_eq!(offered_tool_names(&reqs[0]), vec!["math_add"]);
    let results = tool_messages(&reqs[1]);
    assert_eq!(results.len(), 2, "every call is answered");
    for (id, content) in &results {
        assert!(
            content.contains("unknown tool"),
            "{id} should get an error result, got {content}"
        );
    }
    assert!(run
        .steps
        .iter()
        .any(|s| s.kind == RunStepKind::Error && s.summary.contains("spy_tool")));
}

// ---------- D. tool error is returned to the model ----------

#[tokio::test]
async fn d_tool_error_is_returned_to_model() {
    let (server, rt, tools) = openai_runtime(|req| match tool_messages(&body_of(req)).first() {
        None => tool_call_reply(&[("call_f", "always_fails", "{}")]),
        Some((_, content)) if content.contains("boom") => {
            final_reply("The service is down, please retry later.")
        }
        Some(_) => final_reply("unexpected"),
    })
    .await;

    let run = run(&rt, agent_with_tools(&["always.fails"]), "do it").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(tools.fails.count(), 1);
    assert_eq!(
        run.output.as_ref().unwrap()["text"],
        "The service is down, please retry later."
    );
    let reqs = model_requests(&server).await;
    let (id, content) = &tool_messages(&reqs[1])[0];
    assert_eq!(id, "call_f");
    let err: Value = serde_json::from_str(content).unwrap();
    assert!(err["error"].as_str().unwrap().contains("boom"));
    let tool_step = run
        .steps
        .iter()
        .find(|s| s.kind == RunStepKind::Tool)
        .unwrap();
    assert_eq!(tool_step.detail.as_ref().unwrap()["is_error"], true);
    // Counted as a tool error by Test Lab regression metrics.
    assert!(run
        .steps
        .iter()
        .any(|s| s.kind == RunStepKind::Error && s.summary.contains("tool always.fails failed")));
}

// ---------- E. endless tool loop stops at max_steps ----------

#[tokio::test]
async fn e_max_steps_stops_endless_tool_loop() {
    let (server, rt, tools) = openai_runtime(|req| {
        let n = tool_messages(&body_of(req)).len();
        tool_call_reply(&[(&format!("call_{n}"), "math_add", r#"{"a":1,"b":1}"#)])
    })
    .await;

    let mut agent = agent_with_tools(&["math.add"]);
    agent.limits.max_steps = 3;
    let run = run(&rt, agent, "loop forever").await;

    assert_eq!(run.status, RunStatus::Failed);
    let message = run.error.as_ref().unwrap()["message"].as_str().unwrap();
    assert!(message.contains("max_steps (3)"), "{message}");
    assert_eq!(model_requests(&server).await.len(), 3);
    assert_eq!(tools.add.count(), 3);
    assert!(run.output.is_none());
}

// ---------- F. arguments and tool_call_id round-trip ----------

#[tokio::test]
async fn f_tool_arguments_and_call_id() {
    let args = r#"{"key":"цена","limit":2,"nested":{"ids":[1,2,3]},"flag":true}"#;
    let (server, rt, tools) = openai_runtime(move |req| {
        let results = tool_messages(&body_of(req));
        match results.len() {
            0 => tool_call_reply(&[
                ("call_xyz_789", "kv_lookup", args),
                // Malformed JSON must not reach the tool.
                ("call_bad", "kv_lookup", "{not json"),
            ]),
            _ => final_reply("done"),
        }
    })
    .await;

    let run = run(&rt, agent_with_tools(&["kv.lookup"]), "look up").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(tools.lookup.count(), 1, "only the valid call executes");
    assert_eq!(
        tools.lookup.inputs.lock().unwrap()[0],
        serde_json::from_str::<Value>(args).unwrap()
    );
    let reqs = model_requests(&server).await;
    let results = tool_messages(&reqs[1]);
    assert_eq!(results[0].0, "call_xyz_789");
    assert_eq!(
        serde_json::from_str::<Value>(&results[0].1).unwrap()["key"],
        "цена"
    );
    assert_eq!(results[1].0, "call_bad");
    assert!(results[1].1.contains("not valid JSON"));
    let tool_step = run
        .steps
        .iter()
        .find(|s| s.kind == RunStepKind::Tool)
        .unwrap();
    assert_eq!(
        tool_step.detail.as_ref().unwrap()["call_id"],
        "call_xyz_789"
    );
}

// ---------- G. tools come from agent configuration ----------

#[tokio::test]
async fn g_tools_come_from_agent_config() {
    let (server, rt, _tools) = openai_runtime(|_| final_reply("hi")).await;

    let run_with = run(&rt, agent_with_tools(&["math.add", "kv.lookup"]), "hello").await;
    assert_eq!(run_with.status, RunStatus::Completed);
    let reqs = model_requests(&server).await;
    assert_eq!(offered_tool_names(&reqs[0]), vec!["math_add", "kv_lookup"]);
    let add = &reqs[0]["tools"][0]["function"];
    assert_eq!(add["description"], "Add two integers");
    assert_eq!(add["parameters"]["required"], json!(["a", "b"]));

    // Empty allow-list → no tools offered at all.
    let run_without = run(&rt, agent_with_tools(&[]), "hello").await;
    assert_eq!(run_without.status, RunStatus::Completed);
    let reqs = model_requests(&server).await;
    assert!(reqs[1].get("tools").is_none());

    // Tool needing a permission the agent lacks is withheld, with a trace note.
    let mut agent = agent_with_tools(&["web.search", "math.add"]);
    agent.permissions = Permissions::ai_safe();
    let run_perm = run(&rt, agent, "hello").await;
    let reqs = model_requests(&server).await;
    assert_eq!(offered_tool_names(&reqs[2]), vec!["math_add"]);
    assert!(run_perm
        .steps
        .iter()
        .any(|s| s.summary.starts_with("tool web.search withheld")));

    let mut agent = agent_with_tools(&["web.search"]);
    agent.permissions.allow.insert(Permission::Network);
    run(&rt, agent, "hello").await;
    let reqs = model_requests(&server).await;
    assert_eq!(offered_tool_names(&reqs[3]), vec!["web_search"]);
}

// ---------- H. external input cannot invoke tools ----------

#[tokio::test]
async fn h_event_input_cannot_invoke_tools() {
    let (server, rt, tools) = openai_runtime(|_| final_reply("answered without tools")).await;

    // Even an allowed tool only runs when the model asks for it.
    let agent = agent_with_tools(&["spy.tool"]);
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.upsert_handler(Handler::agent_on_event(
        "webhook",
        "webhook.message.received",
        agent_id,
    ))
    .await
    .unwrap();

    let runs = rt
        .emit(Event::new(
            "webhook.message.received",
            "webhook",
            json!({
                "message": "hi",
                "tools": [
                    { "id": "spy.tool", "input": {} },
                    { "id": "json.echo", "input": { "x": 1 } }
                ]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed, "{:?}", runs[0].error);
    assert_eq!(tools.spy.count(), 0);
    assert!(!runs[0].steps.iter().any(|s| s.kind == RunStepKind::Tool));
    assert!(runs[0]
        .steps
        .iter()
        .any(|s| s.summary.starts_with("input.tools ignored")));
    assert_eq!(model_requests(&server).await.len(), 1);
}

// ---------- timeout covers the model request itself ----------

#[tokio::test]
async fn model_request_is_bounded_by_run_timeout() {
    let (_server, rt, _tools) =
        openai_runtime(|_| final_reply("too late").set_delay(Duration::from_secs(5))).await;

    let mut agent = agent_with_tools(&[]);
    agent.limits.max_runtime_seconds = 1;
    let started = std::time::Instant::now();
    let run = run(&rt, agent, "slow").await;

    assert_eq!(run.status, RunStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(4));
}

// ---------- same loop through Anthropic and Gemini ----------

#[tokio::test]
async fn anthropic_agent_tool_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|req: &Request| {
            let body = body_of(req);
            let last = body["messages"].as_array().unwrap().last().unwrap().clone();
            let result = last["content"]
                .as_array()
                .and_then(|blocks| blocks.iter().find(|b| b["type"] == "tool_result"))
                .cloned();
            let reply = match result {
                None => json!({
                    "content": [
                        { "type": "tool_use", "id": "toolu_a", "name": "math_add",
                          "input": { "a": 20, "b": 22 } },
                        { "type": "tool_use", "id": "toolu_b", "name": "kv_lookup",
                          "input": { "key": "x" } }
                    ],
                    "stop_reason": "tool_use",
                    "usage": { "input_tokens": 10, "output_tokens": 5 }
                }),
                Some(r) => {
                    assert_eq!(r["tool_use_id"], "toolu_a");
                    json!({
                        "content": [{ "type": "text",
                                      "text": format!("Answer: {}", r["content"].as_str().unwrap()) }],
                        "stop_reason": "end_turn",
                        "usage": { "input_tokens": 20, "output_tokens": 5 }
                    })
                }
            };
            ResponseTemplate::new(200).set_body_json(reply)
        })
        .mount(&server)
        .await;

    let provider = Anthropic::with_base_url(
        SecretString::new("sk-ant-test".into()),
        server.uri(),
        HttpClient::new(Default::default()).unwrap(),
    )
    .unwrap();
    let ai = priced(
        AiClient::builder().provider(provider).build().unwrap(),
        ProviderId::anthropic(),
        "claude-test",
    );
    let rt = RouterRuntime::builder()
        .ai(Arc::new(ai))
        .build()
        .await
        .unwrap();
    let tools = register_tools(&rt).await;

    let mut agent = agent_with_tools(&["math.add", "kv.lookup"]);
    agent.model.provider = "anthropic".into();
    agent.model.model = "claude-test".into();
    let run = run(&rt, agent, "20+22?").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(run.output.as_ref().unwrap()["text"], "Answer: 42");
    assert_eq!(tools.add.count(), 1);
    assert_eq!(tools.lookup.count(), 1);
    let reqs = model_requests(&server).await;
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0]["tools"][0]["name"], "math_add");
    let results = reqs[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(results.len(), 2, "both tool_results in one user turn");
}

#[tokio::test]
async fn gemini_agent_tool_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/models/[^/]+:generateContent$"))
        .respond_with(|req: &Request| {
            let body = body_of(req);
            let last = body["contents"].as_array().unwrap().last().unwrap().clone();
            let response = last["parts"][0].get("functionResponse").cloned();
            let reply = match response {
                None => json!({
                    "candidates": [{ "content": { "role": "model", "parts": [
                        { "functionCall": { "name": "math_add", "args": { "a": 1, "b": 6 } } }
                    ]}, "finishReason": "STOP" }]
                }),
                Some(r) => {
                    assert_eq!(r["name"], "math_add");
                    json!({
                        "candidates": [{ "content": { "role": "model", "parts": [
                            { "text": format!("Total {}", r["response"]["content"]) }
                        ]}, "finishReason": "STOP" }],
                        "usageMetadata": { "promptTokenCount": 9, "candidatesTokenCount": 2 }
                    })
                }
            };
            ResponseTemplate::new(200).set_body_json(reply)
        })
        .mount(&server)
        .await;

    let provider = Gemini::with_base_url(
        SecretString::new("AIza-test".into()),
        server.uri(),
        HttpClient::new(Default::default()).unwrap(),
    )
    .unwrap();
    let ai = priced(
        AiClient::builder().provider(provider).build().unwrap(),
        ProviderId::gemini(),
        "gemini-test",
    );
    let rt = RouterRuntime::builder()
        .ai(Arc::new(ai))
        .build()
        .await
        .unwrap();
    let tools = register_tools(&rt).await;

    let mut agent = agent_with_tools(&["math.add"]);
    agent.model.provider = "gemini".into();
    agent.model.model = "gemini-test".into();
    agent.model.max_tokens = Some(321);
    let run = run(&rt, agent, "1+6?").await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(run.output.as_ref().unwrap()["text"], "Total 7");
    assert_eq!(tools.add.count(), 1);
    let reqs = model_requests(&server).await;
    assert_eq!(
        reqs[0]["tools"][0]["functionDeclarations"][0]["name"],
        "math_add"
    );
    assert_eq!(reqs[0]["generationConfig"]["maxOutputTokens"], 321);
}
