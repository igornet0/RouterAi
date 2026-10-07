//! OpenAI provider adapter.

use async_trait::async_trait;
use chrono::Utc;
use reqwest::Method;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use crate::balance::Balance;
use crate::capability::ProviderCapabilities;
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::providers::openai_compatible::{OpenAICompatible, OutputLimitParam};
use crate::types::{ChatRequest, ChatResponse, ChatStream, ProviderId};
use crate::usage::{Usage, UsageReport, UsageRequest};

/// OpenAI API adapter (OpenAI-compatible wire format).
#[derive(Clone, Debug)]
pub struct OpenAI {
    inner: OpenAICompatible,
}

impl OpenAI {
    /// Create with API key (default `https://api.openai.com/v1`).
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Create with shared HTTP client.
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
        let mut caps = ProviderCapabilities::openai_compatible_chat();
        caps.images = true;
        caps.audio = true;
        caps.moderation = true;
        caps.responses = true;
        caps.prompt_caching = true;
        caps.reasoning = true;
        // Costs / usage via Organization Admin API when the key allows it.
        caps.usage = true;
        // No prepaid balance for project/user API keys; see `balance()`.
        caps.balance = false;
        let mut builder = OpenAICompatible::builder()
            .base_url(base_url.unwrap_or("https://api.openai.com/v1"))
            .provider_id(ProviderId::openai())
            .capabilities(caps)
            .output_limit_param(OutputLimitParam::MaxCompletionTokens)
            .http(http);
        if let Some(key) = api_key {
            builder = builder.api_key(key);
        }
        Ok(Self {
            inner: builder.build_unauthenticated()?,
        })
    }

    /// Test helper with custom base URL.
    pub fn with_base_url(
        api_key: impl Into<SecretString>,
        base_url: impl Into<String>,
        http: HttpClient,
    ) -> AiResult<Self> {
        let mut caps = ProviderCapabilities::openai_compatible_chat();
        caps.usage = true;
        let inner = OpenAICompatible::builder()
            .base_url(base_url)
            .api_key(api_key)
            .provider_id(ProviderId::openai())
            .capabilities(caps)
            .output_limit_param(OutputLimitParam::MaxCompletionTokens)
            .http(http)
            .build()?;
        Ok(Self { inner })
    }

    /// Sum organization costs for a window via Admin Costs API.
    ///
    /// Requires an **Admin API key**. Project/user keys return auth errors.
    async fn fetch_organization_costs(
        &self,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
    ) -> AiResult<Decimal> {
        let http = Arc::clone(&self.inner.http);
        let start_unix = start.timestamp().max(0);
        let end_unix = end.timestamp().max(start_unix);
        let url = format!(
            "{}/organization/costs?start_time={start_unix}&end_time={end_unix}&bucket_width=1d&limit=180",
            self.inner.base_url
        );
        let (raw, _, _): (Value, _, _) = http
            .request_json(
                &ProviderId::openai(),
                Method::GET,
                &url,
                Some(self.inner.credential()?),
                &[],
                None::<&()>,
            )
            .await
            .map_err(|err| match err {
                AiError::Authentication { message, provider } => AiError::Authentication {
                    provider,
                    message: format!(
                        "{message} — OpenAI costs need an Admin API key \
                         (platform.openai.com → Organization → Admin keys), \
                         not a project/user secret"
                    ),
                },
                AiError::Authorization { message, provider } => AiError::Authorization {
                    provider,
                    message: format!(
                        "{message} — OpenAI costs need an Admin API key \
                         (platform.openai.com → Organization → Admin keys)"
                    ),
                },
                other => other,
            })?;

        Ok(sum_openai_costs(&raw))
    }
}

/// Sum `amount.value` fields from Organization Costs response buckets.
pub fn sum_openai_costs(raw: &Value) -> Decimal {
    let mut total = Decimal::ZERO;
    let buckets = raw
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for bucket in buckets {
        let results = bucket
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for row in results {
            if let Some(amount) = row.get("amount") {
                if let Some(v) = amount.get("value").and_then(|x| x.as_f64()) {
                    if let Ok(d) = Decimal::from_str(&format!("{v:.6}")) {
                        total += d;
                    }
                } else if let Some(s) = amount.get("value").and_then(|x| x.as_str()) {
                    if let Ok(d) = Decimal::from_str(s) {
                        total += d;
                    }
                }
            }
        }
    }
    total
}

#[async_trait]
impl Provider for OpenAI {
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
        // OpenAI does not expose prepaid remainder via project/user API keys.
        // Dashboard `/dashboard/billing/credit_grants` requires a browser session key
        // — we deliberately do not call it.
        Err(AiError::UnsupportedCapability {
            capability: crate::capability::Capability::Balance,
            provider: Some(self.id()),
        })
    }

    async fn usage(&self, request: UsageRequest) -> AiResult<UsageReport> {
        let cost = self
            .fetch_organization_costs(request.start, request.end)
            .await?;
        Ok(UsageReport {
            usage: Usage::default(),
            cost: Some(cost),
            currency: Some("USD".into()),
            start: request.start,
            end: request.end,
        })
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        match self.list_models().await {
            Ok(_) => Ok(HealthStatus::ok(started.elapsed().as_millis() as u64)),
            Err(err) => Ok(HealthStatus::down(err.to_string())),
        }
    }

    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        Ok(Arc::new(Self {
            inner: self.inner.bound(credential),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_costs_buckets() {
        let raw = json!({
            "data": [{
                "results": [
                    { "amount": { "value": 1.25, "currency": "usd" } },
                    { "amount": { "value": "2.50", "currency": "usd" } }
                ]
            }]
        });
        assert_eq!(
            sum_openai_costs(&raw),
            Decimal::from_str("3.750000").unwrap()
        );
    }
}
