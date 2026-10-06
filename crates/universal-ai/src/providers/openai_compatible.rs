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
use tokio::sync::mpsc;

use crate::capability::{ModelCapabilities, ProviderCapabilities};
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::Provider;
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, Message, ModelId, ProviderId, RequestId,
    Role, StreamEvent, ToolCall,
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
        let base_url = self.base_url.ok_or_else(|| AiError::Config {
            message: "base_url is required".into(),
        })?;
        let api_key = self.api_key.ok_or_else(|| AiError::Config {
            message: "api_key is required".into(),
        })?;
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
    pub(crate) api_key: SecretString,
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
                let mut obj = json!({
                    "role": role,
                    "content": m.content.to_plain_text(),
                });
                if let Some(id) = &m.tool_call_id {
                    obj["tool_call_id"] = json!(id);
                }
                if !m.tool_calls.is_empty() {
                    obj["tool_calls"] = serde_json::to_value(&m.tool_calls).unwrap_or(Value::Null);
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
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = request.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
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
        let message = choice.get("message").ok_or_else(|| AiError::Serialization {
            message: "missing message".into(),
        })?;
        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let tool_calls: Vec<ToolCall> = message
            .get("tool_calls")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
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
        let usage = raw.get("usage").map(|u| Usage {
            prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cached_tokens: u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|v| v.as_u64()),
            reasoning_tokens: u
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(|v| v.as_u64()),
        });

        Ok(ChatResponse {
            request_id,
            model,
            message: Message {
                role: Role::Assistant,
                content: content.into(),
                name: None,
                tool_call_id: None,
                tool_calls,
            },
            finish_reason: finish,
            usage,
            cost: None,
            raw: Some(raw),
        })
    }
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
                Some(&self.api_key),
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
                Some(&self.api_key),
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
                Some(&self.api_key),
                &self.header_refs(),
                Some(&body),
            )
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(AiError::from_http_status(
                self.provider_id.clone(),
                status,
                &text,
                None,
            ));
        }

        let byte_stream = response.bytes_stream();
        let mut es = byte_stream.eventsource();
        let (tx, rx) = mpsc::channel::<Result<StreamEvent, AiError>>(32);

        tokio::spawn(async move {
            while let Some(item) = es.next().await {
                let event = match item {
                    Ok(ev) => ev,
                    Err(e) => {
                        let _ = tx.send(Err(AiError::network(e, true))).await;
                        break;
                    }
                };
                if event.data.trim() == "[DONE]" {
                    let _ = tx.send(Ok(StreamEvent::Done)).await;
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                if let Some(usage) = v.get("usage") {
                    let _ = tx
                        .send(Ok(StreamEvent::Usage {
                            usage: Usage {
                                prompt_tokens: usage
                                    .get("prompt_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0),
                                completion_tokens: usage
                                    .get("completion_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0),
                                total_tokens: usage
                                    .get("total_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0),
                                cached_tokens: None,
                                reasoning_tokens: None,
                            },
                        }))
                        .await;
                }
                let Some(choice) = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                else {
                    continue;
                };
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            let _ = tx
                                .send(Ok(StreamEvent::TextDelta {
                                    text: text.to_string(),
                                }))
                                .await;
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for call in calls {
                            if let Ok(tc) = serde_json::from_value::<ToolCall>(call.clone()) {
                                let _ = tx.send(Ok(StreamEvent::ToolCall { call: tc })).await;
                            }
                        }
                    }
                }
            }
        });

        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Box::pin(stream))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        let result = self
            .http
            .send_raw(
                Method::GET,
                &self.url("models"),
                Some(&self.api_key),
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
}
