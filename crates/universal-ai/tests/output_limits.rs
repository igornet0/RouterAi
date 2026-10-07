//! One output-limit contract for every adapter: `ChatRequest::max_tokens` is the
//! bound on output tokens **including reasoning**, and each adapter sends it in
//! the provider parameter with exactly that meaning — for chat and for streams.
//! A budget-controlled request without any bound is refused before HTTP.

use std::sync::Arc;

use futures::StreamExt;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::http::{HttpClient, HttpConfig};
use universal_ai::{
    AiClient, AiError, Anthropic, BudgetPolicy, DeepSeek, Gemini, KeyId, ModelCapabilities,
    ModelId, ModelInfo, ModelPricing, OpenAI, OpenAICompatible, OpenRouter, Provider,
    ProviderCredential, ProviderId,
};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[derive(Clone, Copy)]
enum Wire {
    /// OpenAI chat-completions body, output bound in this field.
    OpenAi(&'static str),
    Anthropic,
    Gemini,
}

struct Case {
    name: &'static str,
    id: ProviderId,
    wire: Wire,
    streams: bool,
}

const OPENAI_FIELDS: [&str; 2] = ["max_tokens", "max_completion_tokens"];

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "openai",
            id: ProviderId::openai(),
            wire: Wire::OpenAi("max_completion_tokens"),
            streams: true,
        },
        Case {
            name: "openai-compatible",
            id: ProviderId::openai_compatible(),
            wire: Wire::OpenAi("max_tokens"),
            streams: true,
        },
        Case {
            name: "deepseek",
            id: ProviderId::deepseek(),
            wire: Wire::OpenAi("max_tokens"),
            streams: true,
        },
        Case {
            name: "openrouter",
            id: ProviderId::new("openrouter"),
            wire: Wire::OpenAi("max_tokens"),
            streams: true,
        },
        Case {
            name: "anthropic",
            id: ProviderId::anthropic(),
            wire: Wire::Anthropic,
            streams: true,
        },
        Case {
            name: "gemini",
            id: ProviderId::gemini(),
            wire: Wire::Gemini,
            streams: false,
        },
    ]
}

fn http() -> HttpClient {
    HttpClient::new(HttpConfig::default()).unwrap()
}

fn key() -> SecretString {
    SecretString::new("TEST-CREDENTIAL".into())
}

fn reply(wire: Wire, stream: bool) -> ResponseTemplate {
    let sse = |body: &str| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body.to_string())
    };
    match (wire, stream) {
        (Wire::OpenAi(_), false) => ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "role": "assistant", "content": "ok" },
                          "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        })),
        (Wire::OpenAi(_), true) => sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
             data: [DONE]\n\n",
        ),
        (Wire::Anthropic, false) => ResponseTemplate::new(200).set_body_json(json!({
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })),
        (Wire::Anthropic, true) => sse(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
             data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"ok\"}}\n\n\
             data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        ),
        (Wire::Gemini, _) => ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{ "content": { "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2 }
        })),
    }
}

async fn server(wire: Wire) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            reply(wire, body["stream"] == true)
        })
        .mount(&s)
        .await;
    s
}

/// Client with one budgeted provider of `case` pointed at `s`; model "m" priced.
fn client(case: &Case, s: &MockServer) -> AiClient {
    let v1 = format!("{}/v1", s.uri());
    let c = AiClient::builder()
        .allow_empty_providers()
        .budget(BudgetPolicy::daily_usd(Decimal::from(10)))
        .build()
        .unwrap();
    let provider: Arc<dyn Provider> = match case.name {
        "openai" => Arc::new(OpenAI::with_base_url(key(), v1, http()).unwrap()),
        "openai-compatible" => Arc::new(
            OpenAICompatible::builder()
                .base_url(v1)
                .api_key(key())
                .build()
                .unwrap(),
        ),
        "deepseek" => Arc::new(DeepSeek::with_base_url(key(), v1, http()).unwrap()),
        "openrouter" => OpenRouter::new(key())
            .unwrap()
            .with_credential(&ProviderCredential::for_key(
                KeyId::new("k"),
                key(),
                Some(v1),
            ))
            .unwrap(),
        "anthropic" => Arc::new(Anthropic::with_base_url(key(), s.uri(), http()).unwrap()),
        "gemini" => Arc::new(Gemini::with_base_url(key(), s.uri(), http()).unwrap()),
        other => panic!("unknown case {other}"),
    };
    assert_eq!(provider.id(), case.id, "{}", case.name);
    c.register_provider(provider);
    c.pricing().upsert(ModelPricing::per_million(
        case.id.clone(),
        "m",
        Decimal::from(1),
        Decimal::from(2),
    ));
    c
}

/// The output bound the request carried, checking that no other output-limit
/// parameter was sent alongside it.
fn sent_bound(case: &Case, body: &Value) -> Option<u64> {
    match case.wire {
        Wire::OpenAi(field) => {
            for other in OPENAI_FIELDS.iter().filter(|f| **f != field) {
                assert!(
                    body.get(*other).is_none(),
                    "{}: unexpected {other} in {body}",
                    case.name
                );
            }
            body[field].as_u64()
        }
        Wire::Anthropic => body["max_tokens"].as_u64(),
        Wire::Gemini => body["generationConfig"]["maxOutputTokens"].as_u64(),
    }
}

async fn last_body(s: &MockServer) -> Value {
    let reqs = s.received_requests().await.unwrap();
    serde_json::from_slice(&reqs.last().unwrap().body).unwrap()
}

#[tokio::test]
async fn every_adapter_sends_the_bound_in_its_reasoning_inclusive_parameter() {
    for case in cases() {
        let s = server(case.wire).await;
        let c = client(&case, &s);
        let ask = || c.chat().model("m").message("hi").max_tokens(77);

        ask().send().await.unwrap();
        assert_eq!(
            sent_bound(&case, &last_body(&s).await),
            Some(77),
            "{} chat",
            case.name
        );

        if case.streams {
            let mut stream = ask().stream().await.unwrap();
            while let Some(e) = stream.next().await {
                e.unwrap();
            }
            let body = last_body(&s).await;
            assert_eq!(body["stream"], true, "{}", case.name);
            assert_eq!(sent_bound(&case, &body), Some(77), "{} stream", case.name);
        }
    }
}

#[tokio::test]
async fn registry_bound_is_pinned_for_every_adapter() {
    for case in cases() {
        let s = server(case.wire).await;
        let c = client(&case, &s);
        c.models().register(ModelInfo {
            id: ModelId::new("m"),
            provider: case.id.clone(),
            name: None,
            context_window: None,
            max_output_tokens: Some(20),
            capabilities: ModelCapabilities::chat_default(),
            pricing: None,
        });
        c.chat().model("m").message("hi").send().await.unwrap();
        assert_eq!(
            sent_bound(&case, &last_body(&s).await),
            Some(20),
            "{}: the provider cannot exceed what was reserved",
            case.name
        );
    }
}

#[tokio::test]
async fn unbounded_budgeted_request_is_refused_before_http_for_every_adapter() {
    for case in cases() {
        let s = server(case.wire).await;
        let c = client(&case, &s);
        let err = c.chat().model("m").message("hi").send().await.unwrap_err();
        assert!(
            matches!(err, AiError::OutputLimitUnknown { .. }),
            "{}: {err:?}",
            case.name
        );
        if case.streams {
            let err = c
                .chat()
                .model("m")
                .message("hi")
                .stream()
                .await
                .err()
                .unwrap();
            assert!(
                matches!(err, AiError::OutputLimitUnknown { .. }),
                "{}",
                case.name
            );
        }
        assert!(
            s.received_requests().await.unwrap().is_empty(),
            "{}: nothing sent",
            case.name
        );
    }
}
