//! Google Gemini adapter (OpenAI-compatible endpoint when available, plus native generateContent).

use async_trait::async_trait;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use crate::capability::{ModelCapabilities, ProviderCapabilities};
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::Provider;
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, Message, ModelId, ProviderId, RequestId,
    Role,
};
use crate::usage::Usage;

/// Gemini adapter using the Generative Language API.
#[derive(Clone)]
pub struct Gemini {
    api_key: SecretString,
    base_url: String,
    http: Arc<HttpClient>,
}

impl std::fmt::Debug for Gemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gemini")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Gemini {
    /// Create with API key.
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Shared HTTP.
    pub fn with_http(api_key: impl Into<SecretString>, http: HttpClient) -> AiResult<Self> {
        Ok(Self {
            api_key: api_key.into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            http: Arc::new(http),
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
        })
    }
}

#[async_trait]
impl Provider for Gemini {
    fn id(&self) -> ProviderId {
        ProviderId::gemini()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: false, // native stream can be added later
            embeddings: true,
            model_list: true,
            tool_calling: true,
            structured_output: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        let url = format!(
            "{}/models?key={}",
            self.base_url.trim_end_matches('/'),
            self.api_key.expose_secret()
        );
        let (raw, _, _): (Value, _, _) = self
            .http
            .request_json(
                &ProviderId::gemini(),
                Method::GET,
                &url,
                None,
                &[],
                None::<&()>,
            )
            .await?;
        let models = raw
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(models
            .into_iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let id = name.trim_start_matches("models/").to_string();
                Some(ModelInfo {
                    id: ModelId::new(id.clone()),
                    provider: ProviderId::gemini(),
                    name: Some(id),
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities::chat_default(),
                    pricing: None,
                })
            })
            .collect())
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let request_id = RequestId::new();
        let model = request.model.as_str();
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url.trim_end_matches('/'),
            model,
            self.api_key.expose_secret()
        );

        let mut contents = Vec::new();
        let mut system_instruction = None;
        for m in &request.messages {
            match m.role {
                Role::System => system_instruction = Some(m.content.to_plain_text()),
                Role::User => contents.push(json!({
                    "role": "user",
                    "parts": [{"text": m.content.to_plain_text()}]
                })),
                Role::Assistant => contents.push(json!({
                    "role": "model",
                    "parts": [{"text": m.content.to_plain_text()}]
                })),
                Role::Tool => contents.push(json!({
                    "role": "user",
                    "parts": [{"text": m.content.to_plain_text()}]
                })),
            }
        }
        let mut body = json!({ "contents": contents });
        if let Some(sys) = system_instruction {
            body["systemInstruction"] = json!({ "parts": [{"text": sys}] });
        }
        if let Some(t) = request.temperature {
            body["generationConfig"] = json!({ "temperature": t });
        }

        let (raw, _, _): (Value, _, _) = self
            .http
            .request_json(
                &ProviderId::gemini(),
                Method::POST,
                &url,
                None,
                &[],
                Some(&body),
            )
            .await?;

        let text = raw
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        let usage = raw.get("usageMetadata").map(|u| {
            let prompt = u.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0);
            let completion = u
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: u
                    .get("totalTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(prompt + completion),
                cached_tokens: None,
                reasoning_tokens: None,
            }
        });

        Ok(ChatResponse {
            request_id,
            model: request.model,
            message: Message::assistant(text),
            finish_reason: Some(FinishReason::Stop),
            usage,
            cost: None,
            raw: Some(raw),
        })
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AiResult<ChatStream> {
        Err(AiError::UnsupportedCapability {
            capability: crate::capability::Capability::Streaming,
            provider: Some(ProviderId::gemini()),
        })
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        match self.list_models().await {
            Ok(_) => Ok(HealthStatus::ok(started.elapsed().as_millis() as u64)),
            Err(err) => Ok(HealthStatus::down(err.to_string())),
        }
    }
}
