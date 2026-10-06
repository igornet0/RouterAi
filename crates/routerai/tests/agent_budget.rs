//! Budget gate on every model turn of the agent loop.
//!
//! Prices: input $1/M, output $1000/M. Each mock turn reports 10 in / 60 out
//! → actual 0.00001 + 0.06 = **0.06001**. With `max_tokens = 100` a turn's worst
//! case is ≈ 0.1015 (0.1 output + ~1.5k input-bound tokens incl. tool schemas).

use std::sync::Arc;

use routerai::{Agent, RouterRuntime, RunStatus, RunStepKind};
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::{AiClient, ModelId, ModelPricing, OpenAICompatible, ProviderId};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

const TURN_COST: &str = "0.06001";

fn tool_results(req: &Request) -> usize {
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .count()
}

fn usage() -> Value {
    json!({ "prompt_tokens": 10, "completion_tokens": 60, "total_tokens": 70 })
}

fn tool_call(n: usize) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
            { "id": format!("call_{n}"), "type": "function",
              "function": { "name": "json_echo", "arguments": "{\"n\":1}" } }
        ]}, "finish_reason": "tool_calls" }],
        "usage": usage()
    }))
}

fn final_answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{ "message": { "role": "assistant", "content": "done" },
                      "finish_reason": "stop" }],
        "usage": usage()
    }))
}

/// Model asks for a tool until it has seen `tools_before_final` results.
async fn runtime(tools_before_final: usize) -> (MockServer, RouterRuntime) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let n = tool_results(req);
            if n < tools_before_final {
                tool_call(n)
            } else {
                final_answer()
            }
        })
        .mount(&server)
        .await;
    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("KEY_A_TEST".into()))
        .build()
        .unwrap();
    let ai = AiClient::builder().provider(provider).build().unwrap();
    ai.pricing().upsert(ModelPricing {
        provider: ProviderId::openai_compatible(),
        model: ModelId::new("m"),
        input_per_million: Some(d("1")),
        output_per_million: Some(d("1000")),
        cached_input_per_million: None,
        cache_write_per_million: None,
        reasoning_per_million: None,
        effective_from: chrono::Utc::now(),
    });
    let rt = RouterRuntime::builder()
        .ai(Arc::new(ai))
        .build()
        .await
        .unwrap();
    (server, rt)
}

fn agent(max_run_cost: Option<&str>) -> Agent {
    let mut agent = routerai::published_agent("budgeted", "Use json.echo.");
    agent.model.provider = "openai-compatible".into();
    agent.model.model = "m".into();
    agent.model.max_tokens = Some(100);
    agent.tools = vec!["json.echo".into()];
    agent.budget.max_run_cost = max_run_cost.map(d);
    agent.limits.max_steps = 10;
    agent
}

async fn run(rt: &RouterRuntime, agent: Agent) -> routerai::AgentRun {
    let id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.start_run(&id, json!({ "message": "go" }), None)
        .await
        .unwrap()
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn llm_details(run: &routerai::AgentRun) -> Vec<Value> {
    run.steps
        .iter()
        .filter(|s| s.kind == RunStepKind::Llm)
        .map(|s| s.detail.clone().unwrap())
        .collect()
}

#[tokio::test]
async fn e_every_turn_passes_the_budget_gate() {
    // model → tool → model → tool → model
    let (server, rt) = runtime(2).await;
    let run = run(&rt, agent(Some("1"))).await;

    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(hits(&server).await, 3);
    assert_eq!(run.cost, d(TURN_COST) * Decimal::from(3));
    let details = llm_details(&run);
    assert_eq!(details.len(), 3);
    let mut remaining = d("1");
    for detail in &details {
        assert_eq!(detail["budget_decision"], "admitted");
        assert_eq!(detail["cost_status"], "actual");
        let estimated: Decimal = detail["estimated_cost"].as_str().unwrap().parse().unwrap();
        assert!(estimated > d("0.1") && estimated < d("0.11"), "{estimated}");
        // Each turn is capped by what is left of the run budget at that moment.
        let cap: Decimal = detail["remaining_run_budget"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(cap, remaining);
        remaining -= d(TURN_COST);
        assert_eq!(
            detail["charged_cost"]
                .as_str()
                .unwrap()
                .parse::<Decimal>()
                .unwrap(),
            d(TURN_COST)
        );
    }
}

#[tokio::test]
async fn d_max_run_cost_blocks_the_next_turn_before_sending() {
    // Budget fits two turns' worst case but not a third: 0.2 - 2×0.06001 < ~0.1015.
    let (server, rt) = runtime(5).await;
    let run = run(&rt, agent(Some("0.2"))).await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        hits(&server).await,
        2,
        "third model request must not be sent"
    );
    assert_eq!(run.cost, d(TURN_COST) * Decimal::from(2));
    assert!(run.cost <= d("0.2"), "never above max_run_cost");
    let message = run.error.as_ref().unwrap()["message"].as_str().unwrap();
    assert!(message.contains("budget exceeded"), "{message}");
    assert!(message.contains("remaining budget"), "{message}");
    let rejected = llm_details(&run).pop().unwrap();
    assert_eq!(rejected["budget_decision"], "rejected");
    assert_eq!(rejected["reason"], "budget_exceeded");
}

#[tokio::test]
async fn agent_daily_limit_is_enforced_per_agent() {
    let (server, rt) = runtime(0).await;
    let mut limited = agent(None);
    limited.budget.max_daily_cost = Some(d("0.15"));

    let first = run(&rt, limited.clone()).await;
    assert_eq!(first.status, RunStatus::Completed, "{:?}", first.error);
    // 0.06001 spent + ~0.1015 worst case > 0.15 → rejected before sending.
    let second = run(&rt, limited.clone()).await;
    assert_eq!(second.status, RunStatus::Failed);
    let message = second.error.as_ref().unwrap()["message"].as_str().unwrap();
    assert!(message.contains("daily budget exceeded"), "{message}");
    assert!(
        message.contains(&format!("agent:{}", limited.lineage_id)),
        "{message}"
    );
    assert_eq!(hits(&server).await, 1);

    // Another agent has its own scope.
    let other = run(&rt, agent(None)).await;
    assert_eq!(other.status, RunStatus::Completed);
    assert_eq!(hits(&server).await, 2);
}

#[tokio::test]
async fn unknown_price_fails_closed_for_budgeted_agent() {
    let (server, rt) = runtime(0).await;
    let mut a = agent(Some("1"));
    a.model.model = "unpriced".into();
    let run = run(&rt, a).await;

    assert_eq!(run.status, RunStatus::Failed);
    let message = run.error.as_ref().unwrap()["message"].as_str().unwrap();
    assert!(message.contains("pricing is unavailable"), "{message}");
    assert_eq!(hits(&server).await, 0);
    assert_eq!(run.cost, Decimal::ZERO);
}

/// Regression: a turn that needed a retry used to add only the answering
/// attempt's cost to the run; a timed-out (possibly billed) first attempt was
/// invisible to `max_run_cost`.
#[tokio::test]
async fn run_cost_includes_every_attempt_of_a_turn() {
    let server = MockServer::start().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                final_answer().set_delay(std::time::Duration::from_millis(500))
            } else {
                final_answer()
            }
        })
        .mount(&server)
        .await;
    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("KEY_A_TEST".into()))
        .build()
        .unwrap();
    let ai = AiClient::builder()
        .provider(provider)
        .config(universal_ai::AiConfig {
            default_timeout: std::time::Duration::from_millis(100),
            retry_policy: universal_ai::RetryPolicy {
                max_attempts: 2,
                initial_delay: std::time::Duration::from_millis(1),
                ..Default::default()
            },
            ..Default::default()
        })
        .build()
        .unwrap();
    ai.pricing().upsert(ModelPricing::per_million(
        ProviderId::openai_compatible(),
        "m",
        d("1"),
        d("1000"),
    ));
    let rt = RouterRuntime::builder()
        .ai(Arc::new(ai))
        .build()
        .await
        .unwrap();

    let mut a = agent(Some("10"));
    a.tools.clear();
    let run = run(&rt, a).await;
    assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
    assert_eq!(hits(&server).await, 2);
    // Timed-out attempt keeps its worst case; the retry costs the actual amount.
    let details = llm_details(&run);
    let worst: Decimal = details[0]["estimated_cost"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(run.cost, worst + d(TURN_COST));
}
