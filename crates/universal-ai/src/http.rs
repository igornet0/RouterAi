//! Shared HTTP client abstraction.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{header::HeaderMap, Client, Method, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{sanitize_message, AiError, AiResult};
use crate::rate_limit::RateLimit;
use crate::types::ProviderId;

/// HTTP layer configuration.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Full request timeout.
    pub request_timeout: Duration,
    /// Optional proxy URL.
    pub proxy: Option<String>,
    /// Extra default headers.
    pub default_headers: HashMap<String, String>,
    /// User-Agent.
    pub user_agent: String,
    /// Max response body bytes.
    pub max_response_bytes: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            proxy: None,
            default_headers: HashMap::new(),
            user_agent: format!("universal-ai/{}", env!("CARGO_PKG_VERSION")),
            max_response_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Reusable HTTP client (one per process / AiClient).
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: Client,
    config: HttpConfig,
}

impl HttpClient {
    /// Build from config. Certificate validation stays enabled (rustls default).
    pub fn new(config: HttpConfig) -> AiResult<Self> {
        let mut builder = Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .user_agent(&config.user_agent)
            .https_only(false); // allow local http for Ollama etc.; prefer https in providers

        if let Some(proxy) = &config.proxy {
            let proxy = reqwest::Proxy::all(proxy).map_err(|e| AiError::Config {
                message: sanitize_message(&e.to_string()),
            })?;
            builder = builder.proxy(proxy);
        }

        let mut headers = HeaderMap::new();
        for (k, v) in &config.default_headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                AiError::Config {
                    message: format!("invalid header name: {e}"),
                }
            })?;
            let value = reqwest::header::HeaderValue::from_str(v).map_err(|e| AiError::Config {
                message: format!("invalid header value: {e}"),
            })?;
            headers.insert(name, value);
        }
        builder = builder.default_headers(headers);

        let inner = builder.build().map_err(|e| AiError::Network {
            message: sanitize_message(&e.to_string()),
            retryable: false,
        })?;

        Ok(Self { inner, config })
    }

    /// Borrow config.
    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    /// Underlying reqwest client for streaming / advanced use.
    pub fn raw(&self) -> &Client {
        &self.inner
    }

    /// JSON request helper.
    pub async fn request_json<B: Serialize, T: DeserializeOwned>(
        &self,
        provider: &ProviderId,
        method: Method,
        url: &str,
        bearer: Option<&SecretString>,
        extra_headers: &[(&str, &str)],
        body: Option<&B>,
    ) -> AiResult<(T, RateLimit, Option<String>)> {
        let mut req = self.inner.request(method, url);
        if let Some(token) = bearer {
            req = req.bearer_auth(token.expose_secret());
        }
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        let result = async {
            let response = req.send().await.map_err(transport_error)?;
            Self::parse_json_response(provider, response, self.config.max_response_bytes).await
        }
        .await;
        match bearer {
            Some(token) => result.map_err(|e| e.redact_secret(token)),
            None => result,
        }
    }

    /// Send and return raw Response (for SSE).
    pub async fn send_raw(
        &self,
        method: Method,
        url: &str,
        bearer: Option<&SecretString>,
        extra_headers: &[(&str, &str)],
        body: Option<&impl Serialize>,
    ) -> AiResult<Response> {
        let mut req = self.inner.request(method, url);
        if let Some(token) = bearer {
            req = req.bearer_auth(token.expose_secret());
        }
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        req.send().await.map_err(|e| {
            let err = transport_error(e);
            match bearer {
                Some(token) => err.redact_secret(token),
                None => err,
            }
        })
    }

    async fn parse_json_response<T: DeserializeOwned>(
        provider: &ProviderId,
        response: Response,
        max_bytes: usize,
    ) -> AiResult<(T, RateLimit, Option<String>)> {
        let status = response.status();
        let rate = RateLimit::from_headers(response.headers());
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let retry_after = retry_after_secs(response.headers());
        let bytes = response.bytes().await.map_err(transport_error)?;
        if bytes.len() > max_bytes {
            return Err(AiError::Provider {
                details: crate::error::ProviderErrorDetails {
                    provider: provider.clone(),
                    http_status: Some(status.as_u16()),
                    provider_error_code: None,
                    message: format!("response exceeds max size {max_bytes}"),
                    request_id,
                    retryable: false,
                    retry_after_secs: None,
                },
            });
        }

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            return Err(with_retry_after(
                AiError::from_http_status(provider.clone(), status.as_u16(), &body, request_id),
                retry_after,
            ));
        }

        let parsed: T = serde_json::from_slice(&bytes).map_err(|e| AiError::Serialization {
            message: sanitize_message(&e.to_string()),
        })?;
        Ok((parsed, rate, request_id))
    }

    /// Map status helper.
    pub fn ensure_success(provider: &ProviderId, status: StatusCode, body: &str) -> AiResult<()> {
        if status.is_success() {
            Ok(())
        } else {
            Err(AiError::from_http_status(
                provider.clone(),
                status.as_u16(),
                body,
                None,
            ))
        }
    }
}

/// Typed transport error: deadline → [`AiError::Timeout`]; connect / send / body
/// failures → retryable [`AiError::Network`]. Messages are sanitized.
pub(crate) fn transport_error(e: reqwest::Error) -> AiError {
    if e.is_timeout() {
        return AiError::Timeout;
    }
    let retryable = e.is_connect() || e.is_request() || e.is_body();
    AiError::network(e, retryable)
}

/// Typed error for a non-success response: status class, sanitized body,
/// upstream request id and `Retry-After` hint.
pub(crate) async fn error_from_response(provider: &ProviderId, response: Response) -> AiError {
    let status = response.status().as_u16();
    let retry_after = retry_after_secs(response.headers());
    let request_id = response
        .headers()
        .get("x-request-id")
        .or_else(|| response.headers().get("request-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();
    with_retry_after(
        AiError::from_http_status(provider.clone(), status, &body, request_id),
        retry_after,
    )
}

/// `Retry-After` as seconds (delta-seconds or HTTP-date).
fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some((at.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
}

fn with_retry_after(err: AiError, secs: Option<u64>) -> AiError {
    match (err, secs) {
        (
            AiError::RateLimit {
                provider, message, ..
            },
            Some(s),
        ) => AiError::RateLimit {
            provider,
            retry_after_secs: Some(s),
            message,
        },
        (AiError::Provider { mut details }, Some(s)) => {
            details.retry_after_secs = Some(s);
            AiError::Provider { details }
        }
        (err, _) => err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn parses_retry_after_seconds_and_dates() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after_secs(&h), Some(7));
        let soon = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        h.insert("retry-after", HeaderValue::from_str(&soon).unwrap());
        assert!(matches!(retry_after_secs(&h), Some(28..=30)));
        let err = with_retry_after(
            AiError::from_http_status(ProviderId::openai(), 429, "slow down", None),
            Some(3),
        );
        assert_eq!(err.retry_after_secs(), Some(3));
    }
}
