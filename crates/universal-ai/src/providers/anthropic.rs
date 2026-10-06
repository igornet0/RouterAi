//! Anthropic Messages API adapter (not OpenAI wire format).

use async_trait::async_trait;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use crate::capability::ModelCapabilities;
use crate::capability::ProviderCapabilities;
use crate::error::{sanitize_message, AiError, AiResult, ProviderErrorDetails};
use crate::health::HealthStatus;
use crate::http::{error_from_response, transport_error, HttpClient};
use crate::models::ModelInfo;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::providers::openai_compatible::{sse_json, sse_stream};
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, Message, ModelId, ProviderId, RequestId,
    Role, StreamEvent, ToolCall,
};
use crate::usage::Usage;

/// Anthropic Claude adapter.
#[derive(Clone)]
pub struct Anthropic {
    /// Static key, or the key bound for one request; `None` for managed-key templates.
    api_key: Option<SecretString>,
    base_url: String,
    http: Arc<HttpClient>,
    version: String,
}

impl std::fmt::Debug for Anthropic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anthropic")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Anthropic {
    /// Create with API key.
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Shared HTTP.
    pub fn with_http(api_key: impl Into<SecretString>, http: HttpClient) -> AiResult<Self> {
        Ok(Self {
            api_key: Some(api_key.into()),
            base_url: "https://api.anthropic.com".into(),
            http: Arc::new(http),
            version: "2023-06-01".into(),
        })
    }

    /// Keyless template for managed keys (bound per request).
    pub(crate) fn template(base_url: Option<&str>, http: HttpClient) -> AiResult<Self> {
        Ok(Self {
            api_key: None,
            base_url: base_url.unwrap_or("https://api.anthropic.com").into(),
            http: Arc::new(http),
            version: "2023-06-01".into(),
        })
    }

    /// Test helper.
    pub fn with_base_url(
        api_key: impl Into<SecretString>,
        base_url: impl Into<String>,
        http: HttpClient,
    ) -> AiResult<Self> {
        Ok(Self {
            api_key: Some(api_key.into()),
            base_url: base_url.into(),
            http: Arc::new(http),
            version: "2023-06-01".into(),
        })
    }

    /// Credential for this instance; never falls back to another key.
    fn credential(&self) -> AiResult<&SecretString> {
        self.api_key
            .as_ref()
            .ok_or_else(|| AiError::Authentication {
                provider: Some(ProviderId::anthropic()),
                message: "no API key bound to this provider — add an active key".into(),
            })
    }

    fn to_wire(request: &ChatRequest) -> AiResult<Value> {
        let mut system_parts = Vec::new();
        let mut messages: Vec<Value> = Vec::new();
        for m in &request.messages {
            match m.role {
                Role::System => system_parts.push(m.content.to_plain_text()),
                Role::User => {
                    let text = m.content.to_plain_text();
                    if !text.is_empty() {
                        push_blocks(&mut messages, "user", vec![text_block(text)]);
                    }
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();
                    let text = m.content.to_plain_text();
                    if !text.is_empty() {
                        blocks.push(text_block(text));
                    }
                    for call in &m.tool_calls {
                        // `input` must be an object; malformed model arguments were already
                        // answered with an error tool_result, so an empty object is safe here.
                        let input = call
                            .arguments_json()
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                    if !blocks.is_empty() {
                        push_blocks(&mut messages, "assistant", blocks);
                    }
                }
                Role::Tool => {
                    let mut block = json!({
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id,
                        "content": m.content.to_plain_text(),
                    });
                    if m.is_error {
                        block["is_error"] = json!(true);
                    }
                    // Results for one assistant turn share a single user message.
                    push_blocks(&mut messages, "user", vec![block]);
                }
            }
        }
        let mut body = json!({
            "model": request.model.as_str(),
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(1024),
        });
        if !system_parts.is_empty() {
            body["system"] = json!(system_parts.join("\n\n"));
        }
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop.is_empty() {
            body["stop_sequences"] = json!(request.stop);
        }
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    let schema = if t.function.parameters.is_object() {
                        t.function.parameters.clone()
                    } else {
                        json!({ "type": "object" })
                    };
                    let mut tool = json!({
                        "name": t.function.name,
                        "input_schema": schema,
                    });
                    if let Some(desc) = &t.function.description {
                        tool["description"] = json!(desc);
                    }
                    tool
                })
                .collect();
            body["tools"] = json!(tools);
        }
        Ok(body)
    }

    /// Split response content blocks into text + tool calls.
    fn parse_content(raw: &Value) -> (String, Vec<ToolCall>) {
        let mut text = String::new();
        let mut calls = Vec::new();
        for block in raw
            .get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let (Some(id), Some(name)) = (
                        block.get("id").and_then(|v| v.as_str()),
                        block.get("name").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    calls.push(ToolCall::function(id, name, &input));
                }
                _ => {}
            }
        }
        (text, calls)
    }
}

/// Anthropic usage → canonical usage. `input_tokens` excludes cache reads and
/// writes, so the prompt total adds both back; cache writes are their own class
/// (billed above the input rate). `output_tokens` includes extended thinking.
/// Billed units without a canonical class (1-hour cache writes, server tool
/// requests) go to [`Usage::other_tokens`] and make the cost unknown if non-zero.
fn anthropic_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = n("cache_read_input_tokens");
    let cache_write = n("cache_creation_input_tokens");
    let prompt = n("input_tokens") + cache_read + cache_write;
    let completion = n("output_tokens");
    let mut usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: (cache_read > 0).then_some(cache_read),
        cache_creation_tokens: (cache_write > 0).then_some(cache_write),
        reasoning_tokens: None,
        other_tokens: Default::default(),
    };
    // 1-hour cache writes cost more than 5-minute ones; one cache-write rate
    // cannot price them.
    if let Some(one_hour) = u
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(Value::as_u64)
    {
        usage
            .other_tokens
            .insert("cache_creation_1h".into(), one_hour);
    }
    if let Some(tools) = u.get("server_tool_use").and_then(Value::as_object) {
        for (k, v) in tools {
            if let Some(count) = v.as_u64() {
                usage.other_tokens.insert(format!("server_tool_{k}"), count);
            }
        }
    }
    Some(usage)
}

/// Streaming state: `message_start` carries input usage, `message_delta` the
/// final (cumulative) output usage.
#[derive(Default)]
struct AnthropicStreamState {
    usage: Option<Usage>,
}

fn parse_anthropic_event(
    state: &mut AnthropicStreamState,
    v: &Value,
) -> (Vec<AiResult<StreamEvent>>, bool) {
    let etype = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    // TODO(streaming-tool-calls): `tool_use` blocks arrive as
    // content_block_start + input_json_delta fragments; only text deltas are
    // forwarded today. Agents use the non-streaming tool loop.
    match etype {
        "content_block_delta" => {
            let events = v
                .pointer("/delta/text")
                .and_then(|t| t.as_str())
                .map(|text| {
                    vec![Ok(StreamEvent::TextDelta {
                        text: text.to_string(),
                    })]
                })
                .unwrap_or_default();
            (events, false)
        }
        "message_start" => {
            state.usage = v.pointer("/message/usage").and_then(anthropic_usage);
            (Vec::new(), false)
        }
        "message_delta" => {
            let Some(delta) = v.get("usage").and_then(anthropic_usage) else {
                return (Vec::new(), false);
            };
            let mut usage = state.usage.clone().unwrap_or_default();
            // `output_tokens` is cumulative; newer API versions repeat input counts.
            usage.merge_max(&delta);
            usage.completion_tokens = delta.completion_tokens;
            usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
            state.usage = Some(usage.clone());
            (vec![Ok(StreamEvent::Usage { usage })], false)
        }
        "message_stop" => (vec![Ok(StreamEvent::Done)], true),
        "error" => {
            let message = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("stream error");
            let code = v
                .pointer("/error/type")
                .and_then(Value::as_str)
                .map(str::to_string);
            let retryable = matches!(code.as_deref(), Some("overloaded_error" | "api_error"));
            (
                vec![Err(AiError::Provider {
                    details: ProviderErrorDetails {
                        provider: ProviderId::anthropic(),
                        http_status: None,
                        provider_error_code: code,
                        message: sanitize_message(message),
                        request_id: None,
                        retryable,
                        retry_after_secs: None,
                    },
                })],
                true,
            )
        }
        _ => (Vec::new(), false),
    }
}

fn text_block(text: String) -> Value {
    json!({ "type": "text", "text": text })
}

/// Append content blocks, merging into the previous message when the role repeats
/// (Anthropic requires alternating roles; tool results for one turn go together).
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                content.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> ProviderId {
        ProviderId::anthropic()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: true,
            tool_calling: true,
            // `response_format` and cache_control are not mapped by this adapter.
            structured_output: false,
            prompt_caching: false,
            reasoning: true,
            model_list: false,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        // Anthropic does not expose a simple public models list in the same way;
        // return well-known defaults without pretending they came from an API.
        Ok(vec![
            ModelInfo {
                id: ModelId::new("claude-sonnet-4-20250514"),
                provider: ProviderId::anthropic(),
                name: Some("Claude Sonnet 4".into()),
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                capabilities: ModelCapabilities::chat_default(),
                pricing: None,
            },
            ModelInfo {
                id: ModelId::new("claude-3-5-haiku-latest"),
                provider: ProviderId::anthropic(),
                name: Some("Claude 3.5 Haiku".into()),
                context_window: Some(200_000),
                max_output_tokens: Some(8_192),
                capabilities: ModelCapabilities::chat_default(),
                pricing: None,
            },
        ])
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let request_id = RequestId::new();
        let key = self.credential()?;
        let body = Self::to_wire(&request)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let response = self
            .http
            .raw()
            .request(Method::POST, &url)
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport_error(e).redact_secret(key))?;
        if !response.status().is_success() {
            return Err(error_from_response(&ProviderId::anthropic(), response)
                .await
                .redact_secret(key));
        }
        let bytes = response.bytes().await.map_err(transport_error)?;
        let raw: Value = serde_json::from_slice(&bytes).map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;

        let (content, tool_calls) = Self::parse_content(&raw);

        let usage = raw.get("usage").and_then(anthropic_usage);

        let stop = raw
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .map(|s| match s {
                "end_turn" | "stop_sequence" => FinishReason::Stop,
                "max_tokens" => FinishReason::Length,
                "tool_use" => FinishReason::ToolCalls,
                other => FinishReason::Other(other.into()),
            });

        Ok(ChatResponse {
            request_id,
            model: request.model,
            message: Message::assistant_tool_calls(content, tool_calls),
            finish_reason: stop,
            usage,
            cost: None,
            raw: Some(raw),
        })
    }

    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let key = self.credential()?;
        let mut body = Self::to_wire(&request)?;
        body["stream"] = json!(true);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .raw()
            .request(Method::POST, &url)
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport_error(e).redact_secret(key))?;

        if !response.status().is_success() {
            return Err(error_from_response(&ProviderId::anthropic(), response)
                .await
                .redact_secret(key));
        }

        Ok(sse_stream(
            response,
            AnthropicStreamState::default(),
            |state, event| match sse_json(event) {
                Ok(v) => parse_anthropic_event(state, &v),
                Err(err) => (vec![Err(err)], true),
            },
        ))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        // Anthropic has no cheap public ping; report healthy configuration presence.
        let _ = Instant::now();
        if !self
            .api_key
            .as_ref()
            .is_some_and(|k| !k.expose_secret().is_empty())
        {
            Ok(HealthStatus::down("missing api key"))
        } else {
            Ok(HealthStatus::ok(0))
        }
    }

    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        let mut bound = self.clone();
        bound.api_key = Some(credential.secret().clone());
        if let Some(url) = credential.base_url() {
            bound.base_url = url.to_string();
        }
        Ok(Arc::new(bound))
    }
}
