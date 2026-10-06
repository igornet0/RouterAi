//! Token usage tracking.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::types::{AccountId, KeyId, ModelId, ProviderId, RequestId};

/// Token usage for a single request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt / input tokens.
    pub prompt_tokens: u64,
    /// Completion / output tokens.
    pub completion_tokens: u64,
    /// Total tokens when reported.
    pub total_tokens: u64,
    /// Cached prompt tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Compute total if not set.
    pub fn ensure_total(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.prompt_tokens.saturating_add(self.completion_tokens);
        }
        self
    }
}

/// Query for provider usage reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRequest {
    /// Inclusive start.
    pub start: DateTime<Utc>,
    /// Exclusive / inclusive end.
    pub end: DateTime<Utc>,
}

/// Provider-level usage report (optional fields vary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    /// Aggregated usage.
    pub usage: Usage,
    /// Optional monetary amount from provider.
    pub cost: Option<Decimal>,
    /// Currency when cost present.
    pub currency: Option<String>,
    /// Raw window.
    pub start: DateTime<Utc>,
    /// Raw window end.
    pub end: DateTime<Utc>,
}

/// Persisted per-request accounting row (includes full request/response JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestUsage {
    /// Request id.
    pub request_id: RequestId,
    /// Provider.
    pub provider: ProviderId,
    /// Account.
    pub account: AccountId,
    /// API key record id (not secret).
    pub api_key: Option<KeyId>,
    /// Model.
    pub model: ModelId,
    /// Start time.
    pub started_at: DateTime<Utc>,
    /// End time.
    pub finished_at: DateTime<Utc>,
    /// Usage.
    pub usage: Usage,
    /// Cost when calculated.
    pub cost: Option<Cost>,
    /// Success flag.
    pub success: bool,
    /// Latency.
    pub latency_ms: u64,
    /// Full serialized `ChatRequest`.
    #[serde(default = "default_json_object")]
    pub request_json: serde_json::Value,
    /// Full serialized `ChatResponse` when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_json: Option<serde_json::Value>,
    /// Optional operator importance rating (0–10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<u8>,
}

fn default_json_object() -> serde_json::Value {
    serde_json::json!({})
}

/// Validate optional importance score (0–10 inclusive).
pub fn validate_importance(importance: Option<u8>) -> Result<Option<u8>, String> {
    match importance {
        None => Ok(None),
        Some(v) if v <= 10 => Ok(Some(v)),
        Some(v) => Err(format!("importance must be 0..=10, got {v}")),
    }
}

/// Aggregated statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStatistics {
    /// Total requests.
    pub requests: u64,
    /// Successful.
    pub successful_requests: u64,
    /// Failed.
    pub failed_requests: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// Total cost.
    pub total_cost: Decimal,
    /// Average latency.
    pub average_latency_ms: f64,
}

impl UsageStatistics {
    /// Fold a request into aggregates.
    pub fn record(&mut self, row: &RequestUsage) {
        self.requests += 1;
        if row.success {
            self.successful_requests += 1;
        } else {
            self.failed_requests += 1;
        }
        self.input_tokens += row.usage.prompt_tokens;
        self.output_tokens += row.usage.completion_tokens;
        self.total_tokens += row.usage.total_tokens;
        if let Some(cost) = &row.cost {
            self.total_cost += cost.amount;
        }
        let n = self.requests as f64;
        self.average_latency_ms =
            ((self.average_latency_ms * (n - 1.0)) + row.latency_ms as f64) / n;
    }
}

/// In-memory usage manager.
#[derive(Debug, Default)]
pub struct UsageManager {
    rows: std::sync::Mutex<Vec<RequestUsage>>,
}

impl UsageManager {
    /// Create empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed request.
    pub fn record(&self, row: RequestUsage) {
        if let Ok(mut guard) = self.rows.lock() {
            guard.push(row);
        }
    }

    /// Snapshot all rows.
    pub fn list(&self) -> Vec<RequestUsage> {
        self.rows.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Aggregate all.
    pub fn statistics(&self) -> UsageStatistics {
        let mut stats = UsageStatistics::default();
        for row in self.list() {
            stats.record(&row);
        }
        stats
    }

    /// Stats for a calendar day (UTC).
    pub fn statistics_for_day(&self, day: chrono::NaiveDate) -> UsageStatistics {
        let mut stats = UsageStatistics::default();
        for row in self.list() {
            if row.started_at.date_naive() == day {
                stats.record(&row);
            }
        }
        stats
    }

    /// Filter by provider.
    pub fn statistics_by_provider(&self, provider: &ProviderId) -> UsageStatistics {
        let mut stats = UsageStatistics::default();
        for row in self.list() {
            if &row.provider == provider {
                stats.record(&row);
            }
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Currency;

    #[test]
    fn aggregates_usage() {
        let mut stats = UsageStatistics::default();
        let row = RequestUsage {
            request_id: RequestId::new(),
            provider: ProviderId::openai(),
            account: AccountId::new("a"),
            api_key: None,
            model: ModelId::new("gpt"),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: None,
                reasoning_tokens: None,
            },
            cost: Some(Cost {
                currency: Currency::usd(),
                amount: Decimal::new(12, 3),
                input_cost: Decimal::new(4, 3),
                output_cost: Decimal::new(8, 3),
                cache_cost: Decimal::ZERO,
            }),
            success: true,
            latency_ms: 100,
            request_json: serde_json::json!({}),
            response_json: None,
            importance: None,
        };
        stats.record(&row);
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.input_tokens, 10);
        assert_eq!(stats.total_cost, Decimal::new(12, 3));
    }
}
