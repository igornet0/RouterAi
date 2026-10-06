//! Tool / function calling wire format per provider (mock HTTP, no real keys).

use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::http::HttpClient;
use universal_ai::{
    AiClient, Anthropic, ChatRequest, FinishReason, Gemini, Message, OpenAICompatible, Provider,
    Tool, ToolCall, ToolResult,
};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn weather_tool() -> Tool {
    Tool::function(
        "get_weather",
        "Current weather for a city",
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        }),
    )
}

fn ping_tool() -> Tool {
    Tool::function("ping", "No arguments", json!({ "type": "object" }))
}

/// Conversation after the model asked for two tools; the second one failed.
fn conversation_with_results() -> Vec<Message> {
    vec![
        Message::system("You are helpful."),
        Message::system("Answer briefly."),
        Message::user("Weather in Paris and ping?"),
        Message::assistant_tool_calls(
            "",
            vec![
                ToolCall::function("call_w", "get_weather", &json!({ "city": "Paris" })),
                ToolCall::function("call_p", "ping", &json!({})),
            ],
        ),
        Message::tool_result(ToolResult::success("call_w", r#"{"temp_c":21}"#)),
        Message::tool_result(ToolResult::error("call_p", r#"{"error":"ping failed"}"#)),
    ]
}

fn request(messages: Vec<Message>, tools: Vec<Tool>) -> ChatRequest {
    let mut req = ChatRequest::simple("m", "unused");
    req.messages = messages;
    req.tools = tools;
    req
}

async fn last_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.unwrap();
    serde_json::from_slice(&reqs.last().unwrap().body).unwrap()
}

#[tokio::test]
async fn openai_compatible_tools_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call_1", "type": "function",
                          "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } },
                        // Gateway quirks: no type, arguments as an object.
                        { "id": "call_2",
                          "function": { "name": "ping", "arguments": { "n": 1 } } }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .mount(&server)
        .await;

    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("sk-test".into()))
        .build()
        .unwrap();

    // Tools offered through the public ChatBuilder API.
    let client = AiClient::builder()
        .provider(provider.clone())
        .build()
        .unwrap();
    let response = client
        .chat()
        .model("m")
        .message("Weather?")
        .tools([weather_tool()])
        .tool(ping_tool())
        .send()
        .await
        .unwrap();

    let body = last_body(&server).await;
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"][0],
        "city"
    );
    assert_eq!(body["tools"][1]["function"]["name"], "ping");

    assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(
        calls[0].arguments_json().unwrap(),
        json!({ "city": "Paris" })
    );
    assert_eq!(calls[1].id, "call_2");
    assert_eq!(calls[1].call_type, "function");
    assert_eq!(calls[1].arguments_json().unwrap(), json!({ "n": 1 }));

    // History with tool results serializes in OpenAI wire format.
    provider
        .chat(request(conversation_with_results(), vec![weather_tool()]))
        .await
        .unwrap();
    let body = last_body(&server).await;
    let msgs = body["messages"].as_array().unwrap();
    let assistant = &msgs[3];
    assert_eq!(assistant["role"], "assistant");
    assert!(assistant["content"].is_null());
    assert_eq!(assistant["tool_calls"][0]["id"], "call_w");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        r#"{"city":"Paris"}"#
    );
    assert_eq!(msgs[4]["role"], "tool");
    assert_eq!(msgs[4]["tool_call_id"], "call_w");
    assert_eq!(msgs[4]["content"], r#"{"temp_c":21}"#);
    assert_eq!(msgs[5]["tool_call_id"], "call_p");
}

#[tokio::test]
async fn anthropic_tools_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "Checking." },
                { "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                  "input": { "city": "Paris" } },
                { "type": "tool_use", "id": "toolu_2", "name": "ping", "input": {} }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 4 }
        })))
        .mount(&server)
        .await;

    let provider = Anthropic::with_base_url(
        SecretString::new("sk-ant-test".into()),
        server.uri(),
        HttpClient::new(Default::default()).unwrap(),
    )
    .unwrap();

    let response = provider
        .chat(request(
            vec![Message::user("Weather?")],
            vec![weather_tool(), ping_tool()],
        ))
        .await
        .unwrap();
    let body = last_body(&server).await;
    assert_eq!(body["tools"][0]["name"], "get_weather");
    assert_eq!(
        body["tools"][0]["description"],
        "Current weather for a city"
    );
    assert_eq!(body["tools"][0]["input_schema"]["required"][0], "city");

    assert_eq!(response.text(), "Checking.");
    assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 2, "every tool_use block becomes a call");
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].name(), "get_weather");
    assert_eq!(
        calls[0].arguments_json().unwrap(),
        json!({ "city": "Paris" })
    );
    assert_eq!(calls[1].id, "toolu_2");

    provider
        .chat(request(conversation_with_results(), vec![weather_tool()]))
        .await
        .unwrap();
    let body = last_body(&server).await;
    assert_eq!(body["system"], "You are helpful.\n\nAnswer briefly.");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        3,
        "user, assistant(tool_use), user(tool_results)"
    );
    assert_eq!(msgs[1]["role"], "assistant");
    let uses = msgs[1]["content"].as_array().unwrap();
    assert_eq!(uses.len(), 2);
    assert_eq!(uses[0]["type"], "tool_use");
    assert_eq!(uses[0]["id"], "call_w");
    assert_eq!(uses[0]["input"], json!({ "city": "Paris" }));
    assert_eq!(msgs[2]["role"], "user");
    let results = msgs[2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2, "tool results share one user turn");
    assert_eq!(results[0]["type"], "tool_result");
    assert_eq!(results[0]["tool_use_id"], "call_w");
    assert!(results[0].get("is_error").is_none());
    assert_eq!(results[1]["tool_use_id"], "call_p");
    assert_eq!(results[1]["is_error"], true);
}

#[tokio::test]
async fn gemini_function_calling_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/models/[^/]+:generateContent$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "functionCall": { "name": "get_weather", "args": { "city": "Paris" } } },
                        { "functionCall": { "name": "ping", "args": {} } }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 7, "candidatesTokenCount": 3 }
        })))
        .mount(&server)
        .await;

    let provider = Gemini::with_base_url(
        SecretString::new("AIza-test".into()),
        server.uri(),
        HttpClient::new(Default::default()).unwrap(),
    )
    .unwrap();

    let mut req = request(
        vec![Message::user("Weather?")],
        vec![weather_tool(), ping_tool()],
    );
    req.max_tokens = Some(256);
    req.temperature = Some(0.1);
    let response = provider.chat(req).await.unwrap();

    let body = last_body(&server).await;
    let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(decls[0]["parameters"]["required"][0], "city");
    assert!(decls[0]["parameters"].get("additionalProperties").is_none());
    assert!(decls[0]["parameters"].get("$schema").is_none());
    assert!(
        decls[1].get("parameters").is_none(),
        "empty object schema must be omitted for Gemini"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);

    assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name(), "get_weather");
    assert_eq!(
        calls[0].arguments_json().unwrap(),
        json!({ "city": "Paris" })
    );
    assert!(!calls[0].id.is_empty());
    assert_ne!(calls[0].id, calls[1].id, "minted ids must be unique");

    provider
        .chat(request(conversation_with_results(), vec![weather_tool()]))
        .await
        .unwrap();
    let body = last_body(&server).await;
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "You are helpful.\n\nAnswer briefly."
    );
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["functionCall"],
        json!({ "name": "get_weather", "args": { "city": "Paris" } })
    );
    let responses = contents[2]["parts"].as_array().unwrap();
    assert_eq!(responses.len(), 2, "function responses share one user turn");
    assert_eq!(responses[0]["functionResponse"]["name"], "get_weather");
    assert_eq!(
        responses[0]["functionResponse"]["response"],
        json!({ "temp_c": 21 })
    );
    assert_eq!(responses[1]["functionResponse"]["name"], "ping");
    assert_eq!(
        responses[1]["functionResponse"]["response"],
        json!({ "error": "ping failed" })
    );
}
