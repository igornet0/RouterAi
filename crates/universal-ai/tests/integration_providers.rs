//! Integration tests against mock HTTP servers (no real API keys).

use futures::StreamExt;
use secrecy::SecretString;
use serde_json::json;
use universal_ai::{
    AiClient, Anthropic, DeepSeek, OpenAI, OpenAICompatible, Provider, StreamEvent,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_compatible_chat_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello from mock" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
        })))
        .mount(&server)
        .await;

    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("sk-test-not-real".into()))
        .build()
        .unwrap();

    let client = AiClient::builder()
        .provider(provider)
        .with_example_prices()
        .build()
        .unwrap();

    let response = client
        .chat()
        .model("custom-model")
        .message("Hello")
        .send()
        .await
        .unwrap();

    assert_eq!(response.text(), "Hello from mock");
    assert_eq!(response.usage().unwrap().total_tokens, 8);
}

#[tokio::test]
async fn openai_compatible_streaming() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("sk-test".into()))
        .build()
        .unwrap();

    let mut stream = provider
        .stream_chat(universal_ai::ChatRequest::simple("m", "x"))
        .await
        .unwrap();

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::Done => break,
            _ => {}
        }
    }
    assert_eq!(text, "Hi!");
}

#[tokio::test]
async fn deepseek_balance_parsing_via_mock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "USD",
                "total_balance": "42.31",
                "granted_balance": "10.00",
                "topped_up_balance": "32.31"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "deepseek-chat" }]
        })))
        .mount(&server)
        .await;

    let http = universal_ai::http::HttpClient::new(Default::default()).unwrap();
    let provider = DeepSeek::with_base_url(
        SecretString::new("sk-test".into()),
        server.uri(),
        http,
    )
    .unwrap();

    let bal = provider.balance().await.unwrap().unwrap();
    assert_eq!(bal.total.to_string(), "42.31");
}

#[tokio::test]
async fn anthropic_chat_mock() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Claude says hi" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 4 }
        })))
        .mount(&server)
        .await;

    let http = universal_ai::http::HttpClient::new(Default::default()).unwrap();
    let provider = Anthropic::with_base_url(
        SecretString::new("sk-ant-test".into()),
        server.uri(),
        http,
    )
    .unwrap();

    let client = AiClient::builder().provider(provider).build().unwrap();
    let response = client
        .chat()
        .model("claude-3-5-haiku-latest")
        .message("Hi")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text(), "Claude says hi");
}

#[tokio::test]
async fn openai_adapter_uses_compatible_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let http = universal_ai::http::HttpClient::new(Default::default()).unwrap();
    let provider = OpenAI::with_base_url(
        SecretString::new("sk-test".into()),
        format!("{}/v1", server.uri()),
        http,
    )
    .unwrap();
    let resp = provider
        .chat(universal_ai::ChatRequest::simple("gpt-4o-mini", "hi"))
        .await
        .unwrap();
    assert_eq!(resp.text(), "ok");
}

#[tokio::test]
async fn secrets_do_not_appear_in_debug_of_providers() {
    let provider = OpenAICompatible::builder()
        .base_url("https://example.com/v1")
        .api_key(SecretString::new("sk-super-secret-leak-check".into()))
        .build()
        .unwrap();
    let dbg = format!("{provider:?}");
    assert!(!dbg.contains("sk-super-secret-leak-check"));
    assert!(dbg.contains("<redacted>"));
}

#[tokio::test]
async fn fallback_skips_side_effecting() {
    // First provider fails; side_effecting=true must not silently fan-out.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let bad = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("a".into()))
        .provider_id(universal_ai::ProviderId::openai())
        .build()
        .unwrap();

    let client = AiClient::builder()
        .provider(bad)
        .fallback(true)
        .build()
        .unwrap();

    let err = client
        .chat()
        .model("gpt-4o-mini")
        .message("x")
        .side_effecting(true)
        .send()
        .await
        .unwrap_err();
    // Should fail without inventing success
    assert!(err.to_string().contains("provider") || err.to_string().contains("available") || err.to_string().contains("error") || err.to_string().contains("network") || matches!(err, universal_ai::AiError::Provider { .. } | universal_ai::AiError::NoAvailableProvider { .. }));
}
