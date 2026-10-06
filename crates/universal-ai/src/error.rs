//! Unified error types. Secrets must never appear in messages.

use std::fmt;

use thiserror::Error;

use crate::capability::Capability;
use crate::types::ProviderId;

/// Convenient result alias.
pub type AiResult<T> = Result<T, AiError>;

/// Library-wide error.
#[derive(Debug, Error)]
pub enum AiError {
    /// Missing or invalid credentials.
    #[error("authentication failed")]
    Authentication {
        /// Provider context when known.
        provider: Option<ProviderId>,
        /// Safe message (no secrets).
        message: String,
    },

    /// Authenticated but not allowed.
    #[error("authorization failed")]
    Authorization {
        /// Provider context when known.
        provider: Option<ProviderId>,
        /// Safe message.
        message: String,
    },

    /// Rate limited (often retryable).
    #[error("rate limit exceeded")]
    RateLimit {
        /// Provider context when known.
        provider: Option<ProviderId>,
        /// Optional Retry-After seconds.
        retry_after_secs: Option<u64>,
        /// Safe message.
        message: String,
    },

    /// Provider reported insufficient funds.
    #[error("insufficient balance")]
    InsufficientBalance {
        /// Provider context when known.
        provider: Option<ProviderId>,
        /// Safe message.
        message: String,
    },

    /// Local or remote budget policy exceeded.
    #[error("budget exceeded: {message}")]
    BudgetExceeded {
        /// Safe message.
        message: String,
    },

    /// Malformed request / validation.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Safe message.
        message: String,
    },

    /// Model not known or not available.
    #[error("unsupported model: {model}")]
    UnsupportedModel {
        /// Model identifier.
        model: String,
    },

    /// Capability not advertised by provider.
    #[error("unsupported capability: {capability:?}")]
    UnsupportedCapability {
        /// Missing capability.
        capability: Capability,
        /// Provider context when known.
        provider: Option<ProviderId>,
    },

    /// Network / transport failure.
    #[error("network error: {message}")]
    Network {
        /// Safe message.
        message: String,
        /// Whether a retry may help.
        retryable: bool,
    },

    /// Deadline exceeded.
    #[error("timeout")]
    Timeout,

    /// Provider-specific failure with structured details.
    #[error("provider error: {details}")]
    Provider {
        /// Structured provider error.
        details: ProviderErrorDetails,
    },

    /// JSON / wire format issues.
    #[error("serialization error: {message}")]
    Serialization {
        /// Safe message.
        message: String,
    },

    /// Persistence backend failure.
    #[error("storage error: {message}")]
    Storage {
        /// Safe message.
        message: String,
    },

    /// Requested entity was not found.
    #[error("not found: {message}")]
    NotFound {
        /// Safe message.
        message: String,
    },

    /// Secret store failure.
    #[error("secret store error: {message}")]
    SecretStore {
        /// Safe message.
        message: String,
    },

    /// Configuration problem.
    #[error("configuration error: {message}")]
    Config {
        /// Safe message.
        message: String,
    },

    /// No healthy provider left for fallback/routing.
    #[error("no available provider: {message}")]
    NoAvailableProvider {
        /// Safe message.
        message: String,
    },

    /// Catch-all without leaking internals that may contain secrets.
    #[error("unknown error: {message}")]
    Unknown {
        /// Safe message.
        message: String,
    },
}

impl AiError {
    /// Whether the error is safe to retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimit { .. } | Self::Timeout => true,
            Self::Network { retryable, .. } => *retryable,
            Self::Provider { details } => details.retryable,
            Self::Authentication { .. }
            | Self::Authorization { .. }
            | Self::InsufficientBalance { .. }
            | Self::BudgetExceeded { .. }
            | Self::InvalidRequest { .. }
            | Self::UnsupportedModel { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Serialization { .. }
            | Self::Storage { .. }
            | Self::NotFound { .. }
            | Self::SecretStore { .. }
            | Self::Config { .. }
            | Self::NoAvailableProvider { .. }
            | Self::Unknown { .. } => false,
        }
    }

    /// Optional Retry-After hint in seconds.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimit {
                retry_after_secs, ..
            } => *retry_after_secs,
            Self::Provider { details } => details.retry_after_secs,
            _ => None,
        }
    }

    /// Build a sanitized network error from a displayable source.
    pub fn network(err: impl fmt::Display, retryable: bool) -> Self {
        Self::Network {
            message: sanitize_message(&err.to_string()),
            retryable,
        }
    }

    /// Map HTTP status to a typed error (never embeds secrets).
    pub fn from_http_status(
        provider: ProviderId,
        status: u16,
        body: &str,
        request_id: Option<String>,
    ) -> Self {
        let message = truncate_safe(body, 512);
        match status {
            401 => Self::Authentication {
                provider: Some(provider),
                message,
            },
            403 => Self::Authorization {
                provider: Some(provider),
                message,
            },
            408 => Self::Timeout,
            429 => Self::RateLimit {
                provider: Some(provider),
                retry_after_secs: None,
                message,
            },
            400 | 422 => Self::InvalidRequest { message },
            s if (500..600).contains(&s) => Self::Provider {
                details: ProviderErrorDetails {
                    provider,
                    http_status: Some(status),
                    provider_error_code: None,
                    message,
                    request_id,
                    retryable: true,
                    retry_after_secs: None,
                },
            },
            _ => Self::Provider {
                details: ProviderErrorDetails {
                    provider,
                    http_status: Some(status),
                    provider_error_code: None,
                    message,
                    request_id,
                    retryable: false,
                    retry_after_secs: None,
                },
            },
        }
    }
}

/// Structured provider failure metadata.
#[derive(Debug, Clone)]
pub struct ProviderErrorDetails {
    /// Which provider failed.
    pub provider: ProviderId,
    /// HTTP status when applicable.
    pub http_status: Option<u16>,
    /// Provider error code string.
    pub provider_error_code: Option<String>,
    /// Safe human message.
    pub message: String,
    /// Upstream request id if present.
    pub request_id: Option<String>,
    /// Whether retry is appropriate.
    pub retryable: bool,
    /// Optional Retry-After.
    pub retry_after_secs: Option<u64>,
}

impl fmt::Display for ProviderErrorDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "provider={} status={:?} code={:?} request_id={:?} msg={}",
            self.provider,
            self.http_status,
            self.provider_error_code,
            self.request_id,
            self.message
        )
    }
}

/// Strip patterns that look like API keys from free-form text.
pub fn sanitize_message(input: &str) -> String {
    let mut out = input.to_string();
    for prefix in [
        "sk-",
        "sk-ant-",
        "sk-or-",
        "AIza",
        "Bearer ",
        "api_key=",
        "api-key=",
        "Authorization:",
    ] {
        if let Some(idx) = out.find(prefix) {
            let end = out[idx..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .map(|i| idx + i)
                .unwrap_or(out.len());
            out.replace_range(idx..end, "[REDACTED]");
        }
    }
    truncate_safe(&out, 1024)
}

fn truncate_safe(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_api_key_like_strings() {
        let msg = sanitize_message("failed with sk-abcdefghijklmnopqrstuvwxyz123456");
        assert!(!msg.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
        assert!(msg.contains("[REDACTED]"));
    }

    #[test]
    fn auth_errors_are_not_retryable() {
        let err = AiError::Authentication {
            provider: None,
            message: "nope".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn rate_limit_is_retryable() {
        let err = AiError::RateLimit {
            provider: None,
            retry_after_secs: Some(5),
            message: "slow down".into(),
        };
        assert!(err.is_retryable());
        assert_eq!(err.retry_after_secs(), Some(5));
    }
}
