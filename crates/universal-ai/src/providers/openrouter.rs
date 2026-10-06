//! OpenRouter aggregator (OpenAI-compatible).

use async_trait::async_trait;
use secrecy::SecretString;
use std::sync::Arc;

use crate::capability::ProviderCapabilities;
use crate::error::AiResult;
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::providers::openai_compatible::OpenAICompatible;
use crate::types::{ChatRequest, ChatResponse, ChatStream, ProviderId};

/// OpenRouter adapter.
#[derive(Clone, Debug)]
pub struct OpenRouter {
    inner: OpenAICompatible,
}

impl OpenRouter {
    /// Create with API key.
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Shared HTTP.
    pub fn with_http(api_key: impl Into<SecretString>, http: HttpClient) -> AiResult<Self> {
        Self::create(Some(api_key.into()), None, http)
    }

    /// Keyless template for managed keys (bound per request).
    pub(crate) fn template(base_url: Option<&str>, http: HttpClient) -> AiResult<Self> {
        Self::create(None, base_url, http)
    }

    fn create(
        api_key: Option<SecretString>,
        base_url: Option<&str>,
        http: HttpClient,
    ) -> AiResult<Self> {
        let mut builder = OpenAICompatible::builder()
            .base_url(base_url.unwrap_or("https://openrouter.ai/api/v1"))
            .provider_id(ProviderId::openrouter())
            .capabilities(ProviderCapabilities::openai_compatible_chat())
            .http(http)
            .header("HTTP-Referer", "https://github.com/universal-ai")
            .header("X-Title", "universal-ai");
        if let Some(key) = api_key {
            builder = builder.api_key(key);
        }
        Ok(Self {
            inner: builder.build_unauthenticated()?,
        })
    }
}

#[async_trait]
impl Provider for OpenRouter {
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
    async fn health(&self) -> AiResult<HealthStatus> {
        self.inner.health().await
    }

    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        Ok(Arc::new(Self {
            inner: self.inner.bound(credential),
        }))
    }
}
