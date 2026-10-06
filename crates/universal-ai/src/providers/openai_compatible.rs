//! Generic OpenAI-compatible provider (local LLMs, gateways, proxies).

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use reqwest::Method;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use crate::capability::{ModelCapabilities, ProviderCapabilities};
use crate::error::{sanitize_message, AiError, AiResult, ProviderErrorDetails};
use crate::health::HealthStatus;
use crate::http::{error_from_response, transport_error, HttpClient};
use crate::models::ModelInfo;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, FunctionCall, Message, ModelId,
    ProviderId, RequestId, Role, StreamEvent, ToolCall,
};
use crate::usage::Usage;

/// Builder for a generic OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct OpenAICompatibleBuilder {
    base_url: Option<String>,
    api_key: Option<SecretString>,
    provider_id: ProviderId,
    capabilities: ProviderCapabilities,
    http: Option<HttpClient>,
    extra_headers: Vec<(String, String)>,
}

impl std::fmt::Debug for OpenAICompatibleBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatibleBuilder")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("provider_id", &self.provider_id)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl OpenAICompatibleBuilder {
    /// Start builder.
    pub fn new() -> Self {
        Self {
            base_url: None,
            api_key: None,
            provider_id: ProviderId::openai_compatible(),
            capabilities: ProviderCapabilities::openai_compatible_chat(),
            http: None,
            extra_headers: Vec::new(),
        }
    }

    /// Base URL including version prefix, e.g. `https://api.openai.com/v1`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// API key.
    pub fn api_key(mut self, key: impl Into<SecretString>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override provider id.
    pub fn provider_id(mut self, id: ProviderId) -> Self {
        self.provider_id = id;
        self
    }

    /// Override capabilities.
    pub fn capabilities(mut self, caps: ProviderCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Shared HTTP client.
    pub fn http(mut self, http: HttpClient) -> Self {
        self.http = Some(http);
        self
    }

    /// Extra header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Build provider.
    pub fn build(self) -> AiResult<OpenAICompatible> {
        if self.api_key.is_none() {
            return Err(AiError::Config {
                message: "api_key is required".into(),
            });
        }
        self.build_unauthenticated()
    }

    /// Build without a static key: credentials come only from
    /// [`Provider::with_credential`] (managed keys).
    pub(crate) fn build_unauthenticated(self) -> AiResult<OpenAICompatible> {
        let base_url = self.base_url.ok_or_else(|| AiError::Config {
            message: "base_url is required".into(),
        })?;
        let api_key = self.api_key;
        let http = match self.http {
            Some(h) => h,
            None => HttpClient::new(Default::default())?,
        };
        Ok(OpenAICompatible {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            provider_id: self.provider_id,
            capabilities: self.capabilities,
            http: Arc::new(http),
            extra_headers: self.extra_headers,
        })
    }
}

impl Default for OpenAICompatibleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Generic OpenAI-compatible adapter.
#[derive(Clone)]
pub struct OpenAICompatible {
    pub(crate) base_url: String,
    /// Static key from the constructor, or the key bound for one request.
    /// `None` for managed-key templates.
    pub(crate) api_key: Option<SecretString>,
    pub(crate) provider_id: ProviderId,
    pub(crate) capabilities: ProviderCapabilities,
    pub(crate) http: Arc<HttpClient>,
    pub(crate) extra_headers: Vec<(String, String)>,
}

impl std::fmt::Debug for OpenAICompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatible")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

impl OpenAICompatible {
    /// Builder entry.
    pub fn builder() -> OpenAICompatibleBuilder {
        OpenAICompatibleBuilder::new()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Credential for this instance; never falls back to another key.
    pub(crate) fn credential(&self) -> AiResult<&SecretString> {
        self.api_key
            .as_ref()
            .ok_or_else(|| AiError::Authentication {
                provider: Some(self.provider_id.clone()),
                message: "no API key bound to this provider — add an active key".into(),
            })
    }

    /// Copy bound to `credential` (and its endpoint, when the key has one).
    pub(crate) fn bound(&self, credential: &ProviderCredential) -> Self {
        let mut bound = self.clone();
        bound.api_key = Some(credential.secret().clone());
        if let Some(url) = credential.base_url() {
            bound.base_url = url.trim_end_matches('/').to_string();
        }
        bound
    }

    fn header_refs(&self) -> Vec<(&str, &str)> {
        self.extra_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    pub(crate) fn wire_messages(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                let text = m.content.to_plain_text();
                let mut obj = json!({
                    "role": role,
                    "content": text,
                });
                if let Some(id) = &m.tool_call_id {
                    obj["tool_call_id"] = json!(id);
                }
                if !m.tool_calls.is_empty() {
                    obj["tool_calls"] = serde_json::to_value(&m.tool_calls).unwrap_or(Value::Null);
                    if text.is_empty() {
                        // Assistant turns that only call tools carry `content: null`.
                        obj["content"] = Value::Null;
                    }
                }
                obj
            })
            .collect()
    }

    pub(crate) fn wire_body(request: &ChatRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": request.model.as_str(),
            "messages": Self::wire_messages(&request.messages),
            "stream": stream,
        });
        if stream {
            // Without this OpenAI-style APIs send no usage at all in streams.
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = request.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop.is_empty() {
            body["stop"] = json!(request.stop);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::to_value(&request.tools).unwrap_or(Value::Null);
        }
        if let Some(fmt) = &request.response_format {
            body["response_format"] = serde_json::to_value(fmt).unwrap_or(Value::Null);
        }
        body
    }

    pub(crate) fn parse_chat_response(
        &self,
        request_id: RequestId,
        model: ModelId,
        raw: Value,
    ) -> AiResult<ChatResponse> {
        let choice = raw
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| AiError::Serialization {
                message: "missing choices".into(),
            })?;
        let message = choice
            .get("message")
            .ok_or_else(|| AiError::Serialization {
                message: "missing message".into(),
            })?;
        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let tool_calls = message
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|calls| {
                calls
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| parse_wire_tool_call(i, c))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| match s {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "tool_calls" => FinishReason::ToolCalls,
                "content_filter" => FinishReason::ContentFilter,
                other => FinishReason::Other(other.to_string()),
            });
        let usage = raw.get("usage").and_then(parse_wire_usage);

        Ok(ChatResponse {
            request_id,
            model,
            message: Message {
                role: Role::Assistant,
                content: content.into(),
                name: None,
                tool_call_id: None,
                tool_calls,
                is_error: false,
            },
            finish_reason: finish,
            usage,
            cost: None,
            raw: Some(raw),
        })
    }
}

/// OpenAI-style `usage` object (`None` for `null` / missing — never zero tokens).
///
/// `prompt_tokens_details` / `completion_tokens_details` entries other than the
/// known text-priced ones are kept in [`Usage::other_tokens`] (`input_<name>` /
/// `output_<name>`): audio, image or future categories are priced differently,
/// so a non-zero count makes the cost unknown instead of billing it as text.
pub(crate) fn parse_wire_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let n = |ptr: &str| u.pointer(ptr).and_then(|v| v.as_u64());
    let prompt = n("/prompt_tokens").unwrap_or(0);
    let completion = n("/completion_tokens").unwrap_or(0);
    let mut usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: n("/total_tokens").unwrap_or(prompt + completion),
        // DeepSeek reports cache hits as `prompt_cache_hit_tokens`.
        cached_tokens: n("/prompt_tokens_details/cached_tokens")
            .or_else(|| n("/prompt_cache_hit_tokens")),
        cache_creation_tokens: None,
        reasoning_tokens: n("/completion_tokens_details/reasoning_tokens"),
        other_tokens: Default::default(),
    };
    // Categories billed as ordinary text tokens of their parent class.
    const INPUT_TEXT: &[&str] = &["cached_tokens", "text_tokens"];
    // Predicted-output tokens are billed as output tokens (included in completion).
    const OUTPUT_TEXT: &[&str] = &[
        "reasoning_tokens",
        "text_tokens",
        "accepted_prediction_tokens",
        "rejected_prediction_tokens",
    ];
    for (details, prefix, known) in [
        ("prompt_tokens_details", "input", INPUT_TEXT),
        ("completion_tokens_details", "output", OUTPUT_TEXT),
    ] {
        let Some(obj) = u.get(details).and_then(Value::as_object) else {
            continue;
        };
        for (k, v) in obj {
            if known.contains(&k.as_str()) {
                continue;
            }
            if let Some(count) = v.as_u64() {
                let name = k.trim_end_matches("_tokens");
                usage.other_tokens.insert(format!("{prefix}_{name}"), count);
            }
        }
    }
    Some(usage)
}

/// Events carried by one OpenAI-style SSE chunk.
fn parse_stream_chunk(v: &Value, provider: &ProviderId) -> Vec<AiResult<StreamEvent>> {
    let mut out = Vec::new();
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        out.push(Err(AiError::Provider {
            details: ProviderErrorDetails {
                provider: provider.clone(),
                http_status: None,
                provider_error_code: err.get("code").and_then(Value::as_str).map(str::to_string),
                message: sanitize_message(&message),
                request_id: None,
                retryable: false,
                retry_after_secs: None,
            },
        }));
        return out;
    }
    // Non-final chunks carry `"usage": null` when include_usage is on.
    if let Some(usage) = v.get("usage").and_then(parse_wire_usage) {
        out.push(Ok(StreamEvent::Usage { usage }));
    }
    let Some(choice) = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
    else {
        return out;
    };
    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                out.push(Ok(StreamEvent::TextDelta {
                    text: text.to_string(),
                }));
            }
        }
        // TODO(streaming-tool-calls): OpenAI streams tool calls as fragments keyed
        // by `index` (id/name first, then partial `arguments`). This forwards only
        // chunks that parse as a whole call; incremental accumulation is not
        // implemented yet, so agents use the non-streaming tool loop.
        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for call in calls {
                if let Ok(tc) = serde_json::from_value::<ToolCall>(call.clone()) {
                    out.push(Ok(StreamEvent::ToolCall { call: tc }));
                }
            }
        }
    }
    out
}

/// Turn an SSE body into a pull-based [`ChatStream`]: no background task, so
/// dropping the stream drops the HTTP response and closes the connection at once.
/// `parse` maps one SSE event to the stream events it carries and whether the
/// stream ends after them. A transport error or malformed event ends the stream
/// with an error (never silently skipped).
pub(crate) fn sse_stream<S, F>(response: reqwest::Response, state: S, parse: F) -> ChatStream
where
    S: Send + 'static,
    F: FnMut(&mut S, &eventsource_stream::Event) -> (Vec<AiResult<StreamEvent>>, bool)
        + Send
        + 'static,
{
    let es = Box::pin(response.bytes_stream().eventsource());
    let stream = futures::stream::unfold(Some((es, state, parse)), |cursor| async move {
        let (mut es, mut state, mut parse) = cursor?;
        loop {
            match es.next().await {
                None => return None,
                Some(Err(e)) => return Some((vec![Err(sse_error(e))], None)),
                Some(Ok(event)) => {
                    let (events, end) = parse(&mut state, &event);
                    if end {
                        return Some((events, None));
                    }
                    if !events.is_empty() {
                        return Some((events, Some((es, state, parse))));
                    }
                }
            }
        }
    })
    .flat_map(futures::stream::iter);
    Box::pin(stream)
}

fn sse_error(e: eventsource_stream::EventStreamError<reqwest::Error>) -> AiError {
    match e {
        eventsource_stream::EventStreamError::Transport(e) => transport_error(e),
        other => AiError::Serialization {
            message: sanitize_message(&format!("malformed SSE stream: {other}")),
        },
    }
}

/// Parse the JSON payload of an SSE event, or a terminal serialization error.
pub(crate) fn sse_json(event: &eventsource_stream::Event) -> Result<Value, AiError> {
    serde_json::from_str::<Value>(&event.data).map_err(|e| AiError::Serialization {
        message: sanitize_message(&format!("malformed stream chunk: {e}")),
    })
}

/// Parse one `tool_calls[]` entry. Tolerates gateways that omit `type`/`id`
/// or send `arguments` as an object instead of a JSON string.
fn parse_wire_tool_call(index: usize, call: &Value) -> Option<ToolCall> {
    let function = call.get("function")?;
    let name = function.get("name")?.as_str()?.to_string();
    let arguments = match function.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "{}".to_string(),
        Some(other) => other.to_string(),
    };
    let id = call
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{index}"));
    Some(ToolCall {
        id,
        call_type: call
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function")
            .to_string(),
        function: FunctionCall { name, arguments },
    })
}

#[async_trait]
impl Provider for OpenAICompatible {
    fn id(&self) -> ProviderId {
        self.provider_id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelEntry>,
        }
        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
        }

        let (resp, _, _): (ModelsResponse, _, _) = self
            .http
            .request_json(
                &self.provider_id,
                Method::GET,
                &self.url("models"),
                Some(self.credential()?),
                &self.header_refs(),
                None::<&()>,
            )
            .await?;

        Ok(resp
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: ModelId::new(m.id.clone()),
                provider: self.provider_id.clone(),
                name: Some(m.id),
                context_window: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities::chat_default(),
                pricing: None,
            })
            .collect())
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let request_id = RequestId::new();
        let body = Self::wire_body(&request, false);
        let (raw, _rate, _): (Value, _, _) = self
            .http
            .request_json(
                &self.provider_id,
                Method::POST,
                &self.url("chat/completions"),
                Some(self.credential()?),
                &self.header_refs(),
                Some(&body),
            )
            .await?;
        self.parse_chat_response(request_id, request.model, raw)
    }

    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let body = Self::wire_body(&request, true);
        let response = self
            .http
            .send_raw(
                Method::POST,
                &self.url("chat/completions"),
                Some(self.credential()?),
                &self.header_refs(),
                Some(&body),
            )
            .await?;

        if !response.status().is_success() {
            return Err(error_from_response(&self.provider_id, response)
                .await
                .redact_secret(self.credential()?));
        }

        let provider = self.provider_id.clone();
        Ok(sse_stream(response, (), move |_, event| {
            if event.data.trim() == "[DONE]" {
                return (vec![Ok(StreamEvent::Done)], true);
            }
            match sse_json(event) {
                Ok(v) => {
                    let events = parse_stream_chunk(&v, &provider);
                    let failed = events.iter().any(Result::is_err);
                    (events, failed)
                }
                Err(err) => (vec![Err(err)], true),
            }
        }))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        if self.api_key.is_none() {
            return Ok(HealthStatus::down("no API key bound"));
        }
        let result = self
            .http
            .send_raw(
                Method::GET,
                &self.url("models"),
                Some(self.credential()?),
                &self.header_refs(),
                None::<&()>,
            )
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                Ok(HealthStatus::ok(started.elapsed().as_millis() as u64))
            }
            Ok(resp) => Ok(HealthStatus::down(format!("status {}", resp.status()))),
            Err(err) => Ok(HealthStatus::down(err.to_string())),
        }
    }

    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        Ok(Arc::new(self.bound(credential)))
    }
}
