//! Anthropic Messages API adapter (not OpenAI wire format).

use async_trait::async_trait;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use crate::capability::ProviderCapabilities;
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::Provider;
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, Message, ModelId, ProviderId, RequestId,
    Role, StreamEvent,
};
use crate::usage::Usage;
use crate::capability::ModelCapabilities;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio::sync::mpsc;

/// Anthropic Claude adapter.
#[derive(Clone)]
pub struct Anthropic {
    api_key: SecretString,
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
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
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
            api_key: api_key.into(),
            base_url: base_url.into(),
            http: Arc::new(http),
            version: "2023-06-01".into(),
        })
    }

    fn to_wire(request: &ChatRequest) -> AiResult<Value> {
        let mut system = None;
        let mut messages = Vec::new();
        for m in &request.messages {
            match m.role {
                Role::System => {
                    system = Some(m.content.to_plain_text());
                }
                Role::User | Role::Assistant => {
                    let role = if m.role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    };
                    messages.push(json!({
                        "role": role,
                        "content": m.content.to_plain_text(),
                    }));
                }
                Role::Tool => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": m.tool_call_id,
                            "content": m.content.to_plain_text(),
                        }]
                    }));
                }
            }
        }
        let mut body = json!({
            "model": request.model.as_str(),
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(1024),
        });
        if let Some(sys) = system {
            body["system"] = json!(sys);
        }
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        Ok(body)
    }
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
            structured_output: true,
            prompt_caching: true,
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
        let body = Self::to_wire(&request)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let response = self
            .http
            .raw()
            .request(Method::POST, &url)
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::network(e, true))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| AiError::network(e, true))?;
        if !status.is_success() {
            return Err(AiError::from_http_status(
                ProviderId::anthropic(),
                status.as_u16(),
                &String::from_utf8_lossy(&bytes),
                None,
            ));
        }
        let raw: Value = serde_json::from_slice(&bytes).map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;

        let content = raw
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| {
                        if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                            p.get("text").and_then(|t| t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        let usage = raw.get("usage").map(|u| Usage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            total_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cached_tokens: u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64()),
            reasoning_tokens: None,
        });

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
            message: Message::assistant(content),
            finish_reason: stop,
            usage,
            cost: None,
            raw: Some(raw),
        })
    }

    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let mut body = Self::to_wire(&request)?;
        body["stream"] = json!(true);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .raw()
            .request(Method::POST, &url)
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::network(e, true))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(AiError::from_http_status(
                ProviderId::anthropic(),
                status,
                &text,
                None,
            ));
        }

        let mut es = response.bytes_stream().eventsource();
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(item) = es.next().await {
                let Ok(event) = item else {
                    break;
                };
                let Ok(v) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                let etype = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match etype {
                    "content_block_delta" => {
                        if let Some(text) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                            let _ = tx
                                .send(Ok(StreamEvent::TextDelta {
                                    text: text.to_string(),
                                }))
                                .await;
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = v.get("usage") {
                            let _ = tx
                                .send(Ok(StreamEvent::Usage {
                                    usage: Usage {
                                        prompt_tokens: 0,
                                        completion_tokens: usage
                                            .get("output_tokens")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0),
                                        total_tokens: usage
                                            .get("output_tokens")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0),
                                        cached_tokens: None,
                                        reasoning_tokens: None,
                                    },
                                }))
                                .await;
                        }
                    }
                    "message_stop" => {
                        let _ = tx.send(Ok(StreamEvent::Done)).await;
                        break;
                    }
                    _ => {}
                }
            }
        });
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        // Anthropic has no cheap public ping; report healthy configuration presence.
        let _ = Instant::now();
        if self.api_key.expose_secret().is_empty() {
            Ok(HealthStatus::down("missing api key"))
        } else {
            Ok(HealthStatus::ok(0))
        }
    }
}
