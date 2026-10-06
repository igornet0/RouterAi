//! DeepSeek provider — chat via OpenAI-compatible API + balance endpoint.

use async_trait::async_trait;
use reqwest::Method;
use secrecy::SecretString;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

use crate::balance::{parse_deepseek_balance, Balance};
use crate::capability::ProviderCapabilities;
use crate::error::AiResult;
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::Provider;
use crate::providers::openai_compatible::OpenAICompatible;
use crate::types::{ChatRequest, ChatResponse, ChatStream, ProviderId};

/// DeepSeek adapter.
#[derive(Clone)]
pub struct DeepSeek {
    inner: OpenAICompatible,
}

impl std::fmt::Debug for DeepSeek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeek")
            .field("inner", &self.inner)
            .finish()
    }
}

impl DeepSeek {
    /// Create with API key.
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Shared HTTP.
    pub fn with_http(api_key: impl Into<SecretString>, http: HttpClient) -> AiResult<Self> {
        let mut caps = ProviderCapabilities::openai_compatible_chat();
        caps.balance = true;
        caps.reasoning = true;
        let inner = OpenAICompatible::builder()
            .base_url("https://api.deepseek.com")
            .api_key(api_key)
            .provider_id(ProviderId::deepseek())
            .capabilities(caps)
            .http(http)
            .build()?;
        Ok(Self { inner })
    }

    /// For tests against a mock base URL.
    pub fn with_base_url(
        api_key: impl Into<SecretString>,
        base_url: impl Into<String>,
        http: HttpClient,
    ) -> AiResult<Self> {
        let mut caps = ProviderCapabilities::openai_compatible_chat();
        caps.balance = true;
        let inner = OpenAICompatible::builder()
            .base_url(base_url)
            .api_key(api_key)
            .provider_id(ProviderId::deepseek())
            .capabilities(caps)
            .http(http)
            .build()?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl Provider for DeepSeek {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        self.inner.list_models().await
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        self.inner.chat(request).await
    }

    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        self.inner.stream_chat(request).await
    }

    async fn balance(&self) -> AiResult<Option<Balance>> {
        let http = Arc::clone(&self.inner.http);
        let url = format!("{}/user/balance", self.inner.base_url);
        let (raw, _, _): (Value, _, _) = http
            .request_json(
                &ProviderId::deepseek(),
                Method::GET,
                &url,
                Some(&self.inner.api_key),
                &[],
                None::<&()>,
            )
            .await?;
        let balance = parse_deepseek_balance(ProviderId::deepseek(), &raw)?;
        Ok(Some(balance))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        match self.list_models().await {
            Ok(_) => Ok(HealthStatus::ok(started.elapsed().as_millis() as u64)),
            Err(err) => Ok(HealthStatus::down(err.to_string())),
        }
    }
}
