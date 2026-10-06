//! Client configuration, budgets, env loading.

use std::time::Duration;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::retry::RetryPolicy;

/// Global client configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    /// Deadline for each physical attempt (overridable per request with
    /// [`crate::ChatBuilder::timeout`]); the HTTP client has its own
    /// `HttpConfig::request_timeout` as well.
    #[serde(with = "duration_secs")]
    pub default_timeout: Duration,
    /// Retry policy.
    pub retry_policy: RetryPolicy,
    /// Enable automatic fallback across providers.
    pub fallback: bool,
    /// Budget limits.
    pub budget: BudgetPolicy,
    /// Enable telemetry hooks (opt-in).
    pub telemetry_enabled: bool,
    /// Persist prompt / response content in request rows (`request_json` /
    /// `response_json`). `false` keeps only metadata. Content is never logged.
    #[serde(default = "default_true")]
    pub store_request_content: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::default(),
            fallback: false,
            budget: BudgetPolicy::default(),
            telemetry_enabled: false,
            store_request_content: true,
        }
    }
}

/// Spend limits checked **before** requests (fail-closed).
///
/// With any limit set (here, or per request via [`crate::ChatBuilder::max_cost`] /
/// [`crate::ChatBuilder::budget_scope`]) a request is only sent when its worst-case
/// cost is known and fits: unknown model pricing or an unbounded output
/// (`max_tokens` unset, model limit unknown) is rejected. Daily / monthly periods are
/// UTC calendar days / months.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetPolicy {
    /// Max cost per logical request: every attempt's worst case plus what earlier
    /// attempts of the same request were charged.
    pub max_request_cost: Option<Decimal>,
    /// Max daily spend (UTC day).
    pub max_daily_cost: Option<Decimal>,
    /// Max monthly spend (UTC month).
    pub max_monthly_cost: Option<Decimal>,
    /// What to do when a budget-controlled response carries no usage.
    #[serde(default)]
    pub missing_usage: MissingUsagePolicy,
}

/// Handling of responses without token usage under a budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingUsagePolicy {
    /// Keep the worst-case reservation as the charged cost and return the response.
    #[default]
    ChargeReserved,
    /// Charge the reservation and fail with [`crate::AiError::UsageUnavailable`].
    Reject,
}

impl BudgetPolicy {
    /// Whether any global limit is configured.
    pub fn is_limited(&self) -> bool {
        self.max_request_cost.is_some()
            || self.max_daily_cost.is_some()
            || self.max_monthly_cost.is_some()
    }

    /// USD daily budget helper.
    pub fn daily_usd(amount: Decimal) -> Self {
        Self {
            max_daily_cost: Some(amount),
            ..Default::default()
        }
    }
}

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(d: &Duration, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Load API keys from common environment variables (values not logged).
pub fn env_api_key(var: &str) -> Option<secrecy::SecretString> {
    std::env::var(var)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| secrecy::SecretString::new(s.into()))
}

/// Well-known env var names.
pub mod env {
    /// OpenAI.
    pub const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
    /// DeepSeek.
    pub const DEEPSEEK_API_KEY: &str = "DEEPSEEK_API_KEY";
    /// Anthropic.
    pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
    /// Gemini.
    pub const GEMINI_API_KEY: &str = "GEMINI_API_KEY";
    /// OpenRouter.
    pub const OPENROUTER_API_KEY: &str = "OPENROUTER_API_KEY";
}

/// Parse TOML config.
pub fn config_from_toml(s: &str) -> Result<AiConfig, crate::error::AiError> {
    toml::from_str(s).map_err(|e| crate::error::AiError::Config {
        message: e.to_string(),
    })
}

/// Parse JSON config.
pub fn config_from_json(s: &str) -> Result<AiConfig, crate::error::AiError> {
    serde_json::from_str(s).map_err(|e| crate::error::AiError::Config {
        message: e.to_string(),
    })
}
