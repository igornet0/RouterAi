//! Provider wire formats → typed usage → cost, and request validation before
//! any HTTP (mock HTTP servers, fake keys).
//!
//! Prices: input $1000/M, output $2000/M; `message("hi")` + `max_tokens(100)` has a
//! worst case of 0.218.

use futures::StreamExt;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::http::{HttpClient, HttpConfig};
use universal_ai::{
    AiClient, AiError, Anthropic, BudgetPolicy, Capability, Content, ContentPart, CostStatus,
    Gemini, Message, ModelCapabilities, ModelId, ModelInfo, ModelPricing, OpenAICompatible,
    ProviderId, StreamEvent,
};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

const WORST: &str = "0.218";

fn price(provider: ProviderId, model: &str) -> ModelPricing {
    ModelPricing::per_million(provider, model, d("1000"), d("2000"))
}

async fn server(route: &str, reply: ResponseTemplate) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(route))
        .respond_with(reply)
        .mount(&s)
        .await;
    s
}

fn http() -> HttpClient {
    HttpClient::new(HttpConfig::default()).unwrap()
}

fn openai(s: &MockServer) -> OpenAICompatible {
    OpenAICompatible::builder()
        .base_url(format!("{}/v1", s.uri()))
        .api_key(SecretString::new("KEY_TEST".into()))
        .build()
        .unwrap()
}

fn budgeted(provider: impl universal_ai::Provider + 'static, pricing: ModelPricing) -> AiClient {
    let c = AiClient::builder()
        .provider(provider)
        .budget(BudgetPolicy::daily_usd(d("10")))
        .build()
        .unwrap();
    c.pricing().upsert(pricing);
    c
}

async fn ask(c: &AiClient, model: &str) -> Result<universal_ai::ChatResponse, AiError> {
    c.chat()
        .model(model)
        .message("hi")
        .max_tokens(100)
        .send()
        .await
}

fn chat_reply(usage: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{ "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
        "usage": usage,
    }))
}

// ---------- OpenAI-compatible ----------

#[tokio::test]
async fn openai_cached_and_reasoning_tokens_are_priced_per_class() {
    let s = server(
        "/v1/chat/completions",
        chat_reply(json!({
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "prompt_tokens_details": { "cached_tokens": 4 },
            "completion_tokens_details": { "reasoning_tokens": 2 }
        })),
    )
    .await;
    let mut p = price(ProviderId::openai_compatible(), "m");
    p.cached_input_per_million = Some(d("100"));
    let c = budgeted(openai(&s), p);
    let r = ask(&c, "m").await.unwrap();
    let cost = r.cost.unwrap();
    // 6 input × 1000 + 4 cached × 100 + 3 output × 2000 + 2 reasoning × 2000 (per M).
    assert_eq!(cost.input_cost, d("0.006"));
    assert_eq!(cost.cache_cost, d("0.0004"));
    assert_eq!(cost.output_cost, d("0.006"));
    assert_eq!(cost.reasoning_cost, d("0.004"));
    assert_eq!(cost.amount, d("0.0164"));
}

#[tokio::test]
async fn openai_audio_tokens_make_cost_unknown_not_text_priced() {
    let s = server(
        "/v1/chat/completions",
        chat_reply(json!({
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "prompt_tokens_details": { "cached_tokens": 0, "audio_tokens": 3 }
        })),
    )
    .await;
    let c = budgeted(openai(&s), price(ProviderId::openai_compatible(), "m"));
    let r = ask(&c, "m").await.unwrap();
    assert!(r.cost.is_none());
    let row = c.request_usage(&r.request_id).unwrap();
    assert_eq!(row.accounting.status, CostStatus::PricingUnavailable);
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
    assert!(row
        .accounting
        .cost_note
        .as_deref()
        .unwrap()
        .contains("input_audio"));
}

#[tokio::test]
async fn registry_output_bound_is_pinned_into_the_request() {
    let s = server(
        "/v1/chat/completions",
        chat_reply(json!({ "prompt_tokens": 1, "completion_tokens": 1 })),
    )
    .await;
    let c = budgeted(openai(&s), price(ProviderId::openai_compatible(), "m"));
    c.models().register(ModelInfo {
        id: ModelId::new("m"),
        provider: ProviderId::openai_compatible(),
        name: None,
        context_window: None,
        max_output_tokens: Some(20),
        capabilities: ModelCapabilities::chat_default(),
        pricing: None,
    });
    c.chat().model("m").message("hi").send().await.unwrap();
    let body: Value =
        serde_json::from_slice(&s.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(
        body["max_tokens"], 20,
        "provider cannot exceed the reserved bound"
    );
}

// ---------- Anthropic ----------

fn anthropic_reply(usage: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "content": [{ "type": "text", "text": "ok" }],
        "stop_reason": "end_turn",
        "usage": usage,
    }))
}

#[tokio::test]
async fn anthropic_cache_writes_need_their_own_rate() {
    let usage = json!({ "input_tokens": 10, "cache_creation_input_tokens": 5,
                        "cache_read_input_tokens": 0, "output_tokens": 5 });
    let s = server("/v1/messages", anthropic_reply(usage)).await;
    let provider = || Anthropic::with_base_url("KEY_TEST", s.uri(), http()).unwrap();

    // No cache-write rate: unknown (cache writes cost more than input).
    let c = budgeted(provider(), price(ProviderId::anthropic(), "claude-x"));
    let r = ask(&c, "claude-x").await.unwrap();
    assert!(r.cost.is_none());
    let row = c.request_usage(&r.request_id).unwrap();
    assert_eq!(row.accounting.status, CostStatus::PricingUnavailable);
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));

    // With a rate: priced per class.
    let mut p = price(ProviderId::anthropic(), "claude-x");
    p.cache_write_per_million = Some(d("1250"));
    let c = budgeted(provider(), p);
    let cost = ask(&c, "claude-x").await.unwrap().cost.unwrap();
    assert_eq!(cost.cache_write_cost, d("0.00625"));
    assert_eq!(cost.amount, d("0.01") + d("0.00625") + d("0.01"));
}

#[tokio::test]
async fn anthropic_stream_usage_and_error_events() {
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let s = server(
        "/v1/messages",
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse),
    )
    .await;
    let c = budgeted(
        Anthropic::with_base_url("KEY_TEST", s.uri(), http()).unwrap(),
        price(ProviderId::anthropic(), "claude-x"),
    );
    let mut stream = c
        .chat()
        .model("claude-x")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::Actual);
    assert_eq!(row.usage.prompt_tokens, 10);
    assert_eq!(row.usage.completion_tokens, 5);
    assert_eq!(row.accounting.charged_cost, Some(d("0.02")));

    let sse_err = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let s = server(
        "/v1/messages",
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse_err),
    )
    .await;
    let c = budgeted(
        Anthropic::with_base_url("KEY_TEST", s.uri(), http()).unwrap(),
        price(ProviderId::anthropic(), "claude-x"),
    );
    let mut stream = c
        .chat()
        .model("claude-x")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await
        .unwrap();
    let first = stream.next().await.unwrap();
    assert!(matches!(first, Err(AiError::Provider { .. })), "{first:?}");
    assert!(stream.next().await.is_none());
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(
        row.accounting.charged_cost,
        Some(d(WORST)),
        "may have consumed"
    );
}

// ---------- Gemini ----------

#[tokio::test]
async fn gemini_thinking_tokens_are_billed_as_output() {
    let reply = ResponseTemplate::new(200).set_body_json(json!({
        "candidates": [{ "content": { "role": "model", "parts": [{ "text": "ok" }] },
                         "finishReason": "STOP" }],
        "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5,
                           "thoughtsTokenCount": 20, "totalTokenCount": 35 }
    }));
    let s = server("/models/.*", reply).await;
    let c = budgeted(
        Gemini::with_base_url("KEY_TEST", s.uri(), http()).unwrap(),
        price(ProviderId::gemini(), "gem"),
    );
    let r = ask(&c, "gem").await.unwrap();
    let u = r.usage.clone().unwrap();
    assert_eq!((u.completion_tokens, u.reasoning_tokens), (25, Some(20)));
    // 10 × 1000 + 5 × 2000 + 20 × 2000 per M (was 0.02 before: thoughts unbilled).
    assert_eq!(r.cost.unwrap().amount, d("0.06"));
}

#[tokio::test]
async fn gemini_unexplained_total_is_not_priced_as_zero() {
    let reply = ResponseTemplate::new(200).set_body_json(json!({
        "candidates": [{ "content": { "role": "model", "parts": [{ "text": "ok" }] } }],
        "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5,
                           "totalTokenCount": 99 }
    }));
    let s = server("/models/.*", reply).await;
    let c = budgeted(
        Gemini::with_base_url("KEY_TEST", s.uri(), http()).unwrap(),
        price(ProviderId::gemini(), "gem"),
    );
    let r = ask(&c, "gem").await.unwrap();
    assert!(r.cost.is_none());
    assert_eq!(
        c.request_usage(&r.request_id)
            .unwrap()
            .accounting
            .charged_cost,
        Some(d(WORST))
    );
}

// ---------- OpenAI-compatible SSE robustness ----------

async fn sse_client(body: &'static str) -> (MockServer, AiClient) {
    let s = server(
        "/v1/chat/completions",
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body),
    )
    .await;
    let c = budgeted(openai(&s), price(ProviderId::openai_compatible(), "m"));
    (s, c)
}

async fn stream_all(c: &AiClient) -> Vec<Result<StreamEvent, AiError>> {
    let mut s = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    out
}

#[tokio::test]
async fn malformed_chunk_ends_stream_with_error_and_keeps_reservation() {
    let (_s, c) = sse_client(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
data: {not json\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
data: [DONE]\n\n",
    )
    .await;
    let events = stream_all(&c).await;
    assert!(matches!(
        events.last(),
        Some(Err(AiError::Serialization { .. }))
    ));
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
}

#[tokio::test]
async fn null_and_duplicate_usage_chunks() {
    let (_s, c) = sse_client(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}],\"usage\":null}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
data: [DONE]\n\n",
    )
    .await;
    stream_all(&c).await;
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::Actual);
    assert_eq!(
        row.accounting.charged_cost,
        Some(d("0.02")),
        "duplicates not summed"
    );
}

#[tokio::test]
async fn in_band_stream_error_is_surfaced() {
    let (_s, c) =
        sse_client("data: {\"error\":{\"message\":\"upstream failed\",\"code\":\"x\"}}\n\n").await;
    let events = stream_all(&c).await;
    assert!(matches!(events[0], Err(AiError::Provider { .. })));
    assert_eq!(events.len(), 1);
}

// ---------- validation before HTTP ----------

#[tokio::test]
async fn unsupported_requests_fail_before_http() {
    let s = server("/v1/chat/completions", chat_reply(json!({}))).await;
    let gem = server("/models/.*", ResponseTemplate::new(200)).await;
    let c = budgeted(openai(&s), price(ProviderId::openai_compatible(), "m"));

    // Image parts would be silently dropped by the text-only wire format.
    let image = Message::user(Content::Parts(vec![
        ContentPart::text("what is this?"),
        ContentPart::ImageUrl {
            url: "https://example.com/x.png".into(),
        },
    ]));
    let err = c
        .chat()
        .model("m")
        .add_message(image)
        .max_tokens(10)
        .send()
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AiError::UnsupportedCapability {
            capability: Capability::Images,
            ..
        }
    ));

    // Output limit of a registered model.
    c.models().register(ModelInfo {
        id: ModelId::new("m"),
        provider: ProviderId::openai_compatible(),
        name: None,
        context_window: None,
        max_output_tokens: Some(50),
        capabilities: ModelCapabilities {
            streaming: false,
            ..ModelCapabilities::chat_default()
        },
        pricing: None,
    });
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(51)
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::InvalidRequest { .. }), "{err:?}");

    // Model registered without streaming.
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(10)
        .stream()
        .await
        .err()
        .unwrap();
    assert!(matches!(
        err,
        AiError::UnsupportedCapability {
            capability: Capability::Streaming,
            ..
        }
    ));
    assert_eq!(s.received_requests().await.unwrap().len(), 0);

    // Provider without streaming support (Gemini adapter).
    let g = budgeted(
        Gemini::with_base_url("KEY_TEST", gem.uri(), http()).unwrap(),
        price(ProviderId::gemini(), "gem"),
    );
    let err = g
        .chat()
        .model("gem")
        .message("hi")
        .max_tokens(10)
        .stream()
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), universal_ai::ErrorKind::Validation);
    assert_eq!(gem.received_requests().await.unwrap().len(), 0);
}
