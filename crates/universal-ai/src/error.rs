//! Unified error types. Secrets must never appear in messages.

use std::fmt;

use thiserror::Error;

use rust_decimal::Decimal;

use crate::capability::Capability;
use crate::types::{ModelId, ProviderId};

/// Convenient result alias.
pub type AiResult<T> = Result<T, AiError>;

/// Machine-readable error class (stable, serializable). Use [`AiError::kind`]
/// instead of matching on message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// Local spend limit (per request / daily / monthly / per run).
    Budget,
    /// Cost cannot be determined or bounded (no price, no output bound).
    Pricing,
    /// Provider returned no usage for a budget-controlled request.
    Usage,
    /// Provider rate limit (HTTP 429).
    RateLimit,
    /// Provider account quota / prepaid balance exhausted.
    ProviderQuota,
    /// Provider-side failure (5xx, malformed response, other statuses).
    Provider,
    /// Transport failure.
    Network,
    /// Deadline exceeded.
    Timeout,
    /// Cancelled by the caller.
    Cancellation,
    /// Credentials missing / rejected / not permitted.
    Authentication,
    /// Invalid request or unsupported model / capability (rejected before sending
    /// when detectable locally).
    Validation,
    /// Invalid configuration.
    Configuration,
    /// Persistence or secret-store failure.
    Storage,
    /// Requested entity does not exist.
    NotFound,
    /// Unexpected internal failure.
    Internal,
}

/// Library-wide error.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// A budget-controlled request needs a known price for its model.
    #[error("model pricing is unavailable for {provider}/{model}")]
    PricingUnavailable {
        /// Provider that would serve the request.
        provider: ProviderId,
        /// Requested model.
        model: ModelId,
    },

    /// Worst-case output cost cannot be bounded (no `max_tokens`, unknown model limit).
    #[error("cannot bound output cost for {model}: set max_tokens")]
    OutputLimitUnknown {
        /// Requested model.
        model: ModelId,
    },

    /// Daily spend limit would be exceeded (UTC day).
    #[error(
        "daily budget exceeded ({}): spent {spent} + estimated {requested} > limit {limit}",
        .scope.as_deref().unwrap_or("global")
    )]
    DailyLimitExceeded {
        /// Budget scope (`None` = global).
        scope: Option<String>,
        /// Already committed (settled + reserved) today.
        spent: Decimal,
        /// Worst-case estimate of the rejected request.
        requested: Decimal,
        /// Configured limit.
        limit: Decimal,
    },

    /// Monthly spend limit would be exceeded (UTC month).
    #[error(
        "monthly budget exceeded ({}): spent {spent} + estimated {requested} > limit {limit}",
        .scope.as_deref().unwrap_or("global")
    )]
    MonthlyLimitExceeded {
        /// Budget scope (`None` = global).
        scope: Option<String>,
        /// Already committed (settled + reserved) this month.
        spent: Decimal,
        /// Worst-case estimate of the rejected request.
        requested: Decimal,
        /// Configured limit.
        limit: Decimal,
    },

    /// Provider returned no usage for a budget-controlled request.
    #[error("usage unavailable from {provider} for {model}; reserved worst-case cost was charged")]
    UsageUnavailable {
        /// Provider.
        provider: ProviderId,
        /// Model.
        model: ModelId,
    },

    /// Local budget policy exceeded (per-request / per-run cap).
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

    /// The caller cancelled the request before it finished.
    #[error("request cancelled")]
    Cancelled,

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
    /// Machine-readable classification.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Authentication { .. } | Self::Authorization { .. } => ErrorKind::Authentication,
            Self::RateLimit { .. } => ErrorKind::RateLimit,
            Self::InsufficientBalance { .. } => ErrorKind::ProviderQuota,
            Self::PricingUnavailable { .. } | Self::OutputLimitUnknown { .. } => ErrorKind::Pricing,
            Self::DailyLimitExceeded { .. }
            | Self::MonthlyLimitExceeded { .. }
            | Self::BudgetExceeded { .. } => ErrorKind::Budget,
            Self::UsageUnavailable { .. } => ErrorKind::Usage,
            Self::InvalidRequest { .. }
            | Self::UnsupportedModel { .. }
            | Self::UnsupportedCapability { .. } => ErrorKind::Validation,
            Self::Network { .. } => ErrorKind::Network,
            Self::Timeout => ErrorKind::Timeout,
            Self::Cancelled => ErrorKind::Cancellation,
            Self::Provider { .. }
            | Self::Serialization { .. }
            | Self::NoAvailableProvider { .. } => ErrorKind::Provider,
            Self::Storage { .. } | Self::SecretStore { .. } => ErrorKind::Storage,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Config { .. } => ErrorKind::Configuration,
            Self::Unknown { .. } => ErrorKind::Internal,
        }
    }

    /// Whether retrying the **same provider** may help. Each retry is a new physical
    /// attempt with its own reservation, so a retry is never financially hidden.
    /// Budget, pricing, usage, validation, authentication and configuration errors
    /// are never retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimit { .. } | Self::Timeout => true,
            Self::Network { retryable, .. } => *retryable,
            Self::Provider { details } => details.retryable,
            _ => false,
        }
    }

    /// Whether trying **another provider** may help (availability / account
    /// problems specific to the failed provider). Local budget / pricing / usage
    /// decisions, invalid requests and cancellation never fall back.
    pub fn is_fallbackable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit { .. }
                | Self::InsufficientBalance { .. }
                | Self::Provider { .. }
                | Self::Serialization { .. }
                | Self::NoAvailableProvider { .. }
                | Self::Network { .. }
                | Self::Timeout
                | Self::Authentication { .. }
                | Self::Authorization { .. }
                // Key binding for this provider failed (missing secret, adapter
                // without managed-key support): another provider may be usable.
                | Self::SecretStore { .. }
                | Self::Config { .. }
        )
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

    /// Machine-readable reason for budget / pricing failures (`None` otherwise).
    pub fn budget_reason(&self) -> Option<&'static str> {
        match self {
            Self::PricingUnavailable { .. } => Some("pricing_unavailable"),
            Self::OutputLimitUnknown { .. } => Some("output_limit_unknown"),
            Self::DailyLimitExceeded { .. } => Some("daily_limit_exceeded"),
            Self::MonthlyLimitExceeded { .. } => Some("monthly_limit_exceeded"),
            Self::UsageUnavailable { .. } => Some("usage_unavailable"),
            Self::BudgetExceeded { .. } => Some("budget_exceeded"),
            _ => None,
        }
    }

    /// Whether a failed call may still have reached the model and consumed tokens.
    /// `true` keeps the attempt's reservation charged (unknown cost is not zero):
    /// timeouts, transport errors, malformed responses, cancellation, and gateway
    /// statuses that do not prove the upstream did nothing (502 / 504 / 524).
    /// Definitive provider rejections (4xx, 429, 500, 503) consumed nothing.
    pub fn may_have_consumed_tokens(&self) -> bool {
        match self {
            Self::Timeout
            | Self::Cancelled
            | Self::Network { .. }
            | Self::Serialization { .. }
            | Self::Unknown { .. } => true,
            Self::Provider { details } => {
                matches!(details.http_status, None | Some(502 | 504 | 524))
            }
            _ => false,
        }
    }

    /// Replace every occurrence of `secret` in error text. Providers sometimes echo
    /// the credential back (e.g. "invalid key: …"), which pattern-based
    /// [`sanitize_message`] cannot recognize for arbitrary key formats.
    pub fn redact_secret(self, secret: &secrecy::SecretString) -> Self {
        use secrecy::ExposeSecret;
        let value = secret.expose_secret();
        if value.len() < 4 {
            return self;
        }
        let r = |m: String| m.replace(value, "[REDACTED]");
        match self {
            Self::Authentication { provider, message } => Self::Authentication {
                provider,
                message: r(message),
            },
            Self::Authorization { provider, message } => Self::Authorization {
                provider,
                message: r(message),
            },
            Self::RateLimit {
                provider,
                retry_after_secs,
                message,
            } => Self::RateLimit {
                provider,
                retry_after_secs,
                message: r(message),
            },
            Self::InsufficientBalance { provider, message } => Self::InsufficientBalance {
                provider,
                message: r(message),
            },
            Self::BudgetExceeded { message } => Self::BudgetExceeded {
                message: r(message),
            },
            Self::InvalidRequest { message } => Self::InvalidRequest {
                message: r(message),
            },
            Self::Network { message, retryable } => Self::Network {
                message: r(message),
                retryable,
            },
            Self::Provider { mut details } => {
                details.message = r(details.message);
                Self::Provider { details }
            }
            Self::Serialization { message } => Self::Serialization {
                message: r(message),
            },
            Self::Storage { message } => Self::Storage {
                message: r(message),
            },
            Self::NotFound { message } => Self::NotFound {
                message: r(message),
            },
            Self::SecretStore { message } => Self::SecretStore {
                message: r(message),
            },
            Self::Config { message } => Self::Config {
                message: r(message),
            },
            Self::NoAvailableProvider { message } => Self::NoAvailableProvider {
                message: r(message),
            },
            Self::Unknown { message } => Self::Unknown {
                message: r(message),
            },
            other @ (Self::UnsupportedModel { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Timeout
            | Self::Cancelled
            | Self::PricingUnavailable { .. }
            | Self::OutputLimitUnknown { .. }
            | Self::DailyLimitExceeded { .. }
            | Self::MonthlyLimitExceeded { .. }
            | Self::UsageUnavailable { .. }) => other,
        }
    }

    /// Map HTTP status to a typed error (never embeds secrets).
    pub fn from_http_status(
        provider: ProviderId,
        status: u16,
        body: &str,
        request_id: Option<String>,
    ) -> Self {
        let message = truncate_safe(&sanitize_message(body), 512);
        match status {
            401 => Self::Authentication {
                provider: Some(provider),
                message,
            },
            402 => Self::InsufficientBalance {
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
            // 501 / 505: the endpoint will not serve this request — not transient.
            s if (500..600).contains(&s) && s != 501 && s != 505 => Self::Provider {
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

/// Strip patterns that look like API keys from free-form text (every occurrence).
pub fn sanitize_message(input: &str) -> String {
    let mut out = input.to_string();
    for prefix in [
        "sk-ant-",
        "sk-or-",
        "sk-",
        "AIza",
        "Bearer ",
        "api_key=",
        "api-key=",
        "x-api-key:",
        "x-goog-api-key:",
        "Authorization:",
    ] {
        let mut from = 0;
        while let Some(pos) = out[from..].find(prefix) {
            let idx = from + pos;
            let value_start = idx + prefix.len();
            let end = out[value_start..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '&' | '}'))
                .map(|i| value_start + i)
                .unwrap_or(out.len());
            // `Authorization: Bearer x` — the value after the header name starts
            // with a space; skip it so the token itself is redacted too.
            let end = if end == value_start {
                let rest = &out[value_start..];
                let trimmed = rest.trim_start();
                let skip = rest.len() - trimmed.len();
                trimmed
                    .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ','))
                    .map(|i| value_start + skip + i)
                    .unwrap_or(out.len())
            } else {
                end
            };
            out.replace_range(idx..end, "[REDACTED]");
            from = idx + "[REDACTED]".len();
            if from >= out.len() {
                break;
            }
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
    fn sanitizes_every_occurrence() {
        let msg = sanitize_message(
            "a sk-AAAAAAAAAAAAAAAA b sk-BBBBBBBBBBBBBBBB Authorization: Bearer tok123 api_key=zzz&x=1",
        );
        for leaked in ["AAAAAAAA", "BBBBBBBB", "tok123", "zzz"] {
            assert!(!msg.contains(leaked), "{leaked} leaked in {msg}");
        }
    }

    #[test]
    fn http_error_bodies_are_sanitized() {
        let err = AiError::from_http_status(
            ProviderId::openai(),
            401,
            "{\"error\":\"Incorrect API key provided: sk-proj-SECRETSECRET\"}",
            None,
        );
        assert!(!format!("{err:?}").contains("SECRETSECRET"));
    }

    #[test]
    fn retry_fallback_and_financial_classification() {
        let p = |s: u16| AiError::from_http_status(ProviderId::openai(), s, "x", None);
        // (status, retryable, fallbackable, may_have_consumed)
        for (status, retry, fallback, consumed) in [
            (400, false, false, false),
            (401, false, true, false),
            (402, false, true, false),
            (403, false, true, false),
            (408, true, true, true),
            (422, false, false, false),
            (429, true, true, false),
            (500, true, true, false),
            (501, false, true, false),
            (502, true, true, true),
            (503, true, true, false),
            (504, true, true, true),
        ] {
            let e = p(status);
            assert_eq!(e.is_retryable(), retry, "{status} retry");
            assert_eq!(e.is_fallbackable(), fallback, "{status} fallback");
            assert_eq!(e.may_have_consumed_tokens(), consumed, "{status} consumed");
        }
        for e in [
            AiError::PricingUnavailable {
                provider: ProviderId::openai(),
                model: ModelId::new("m"),
            },
            AiError::OutputLimitUnknown {
                model: ModelId::new("m"),
            },
            AiError::DailyLimitExceeded {
                scope: None,
                spent: Decimal::ZERO,
                requested: Decimal::ONE,
                limit: Decimal::ONE,
            },
            AiError::UsageUnavailable {
                provider: ProviderId::openai(),
                model: ModelId::new("m"),
            },
            AiError::Config {
                message: "x".into(),
            },
            AiError::Cancelled,
        ] {
            assert!(!e.is_retryable(), "{e:?} must not be retried");
        }
        assert!(!AiError::Cancelled.is_fallbackable());
        assert_eq!(AiError::Timeout.kind(), ErrorKind::Timeout);
        assert!(AiError::Serialization {
            message: "bad json".into()
        }
        .may_have_consumed_tokens());
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
