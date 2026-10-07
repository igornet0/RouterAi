//! Token usage tracking.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::error::ErrorKind;
use crate::types::{AccountId, KeyId, ModelId, ProviderId, RequestId};

/// Token usage for a single physical attempt, as reported by the provider and
/// normalized by its adapter.
///
/// Canonical semantics (every adapter maps its wire format onto these):
///
/// * `prompt_tokens` — **all** input tokens, including cache reads
///   (`cached_tokens`) and cache writes (`cache_creation_tokens`).
/// * `completion_tokens` — **all** output tokens, including `reasoning_tokens`.
/// * `other_tokens` — provider-reported categories without a canonical class
///   (audio, image, …). Any non-zero entry makes the cost
///   [`CostStatus::PricingUnavailable`]: there is no silent fallback to the text rate.
///
/// See [`Usage::breakdown`] for the disjoint billing classes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// All input tokens (including cache reads and cache writes).
    pub prompt_tokens: u64,
    /// All output tokens (including reasoning).
    pub completion_tokens: u64,
    /// Total tokens when reported.
    pub total_tokens: u64,
    /// Input tokens served from the provider's prompt cache (subset of `prompt_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Input tokens written to the provider's prompt cache (subset of `prompt_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Reasoning / thinking tokens (subset of `completion_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-specific token categories with no canonical class (e.g.
    /// `input_audio`, `output_image`). Non-zero values cannot be priced.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other_tokens: BTreeMap<String, u64>,
}

/// Disjoint billing classes derived from a [`Usage`] (see [`Usage::breakdown`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    /// Uncached input tokens.
    pub input: u64,
    /// Input tokens read from the prompt cache.
    pub cached_input: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation: u64,
    /// Non-reasoning output tokens.
    pub output: u64,
    /// Reasoning output tokens.
    pub reasoning: u64,
}

impl Usage {
    /// Compute total if not set.
    pub fn ensure_total(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.prompt_tokens.saturating_add(self.completion_tokens);
        }
        self
    }

    /// Split into disjoint billing classes. Fails when the subsets do not fit their
    /// totals (e.g. more cached than prompt tokens): such usage cannot be trusted
    /// for billing and is treated as unknown cost, never clamped.
    pub fn breakdown(&self) -> Result<TokenBreakdown, String> {
        let cached = self.cached_tokens.unwrap_or(0);
        let creation = self.cache_creation_tokens.unwrap_or(0);
        let reasoning = self.reasoning_tokens.unwrap_or(0);
        let input = self
            .prompt_tokens
            .checked_sub(cached)
            .and_then(|v| v.checked_sub(creation))
            .ok_or_else(|| {
                format!(
                    "inconsistent usage: cached ({cached}) + cache creation ({creation}) \
                     exceed prompt tokens ({})",
                    self.prompt_tokens
                )
            })?;
        let output = self
            .completion_tokens
            .checked_sub(reasoning)
            .ok_or_else(|| {
                format!(
                    "inconsistent usage: reasoning ({reasoning}) exceeds completion tokens ({})",
                    self.completion_tokens
                )
            })?;
        Ok(TokenBreakdown {
            input,
            cached_input: cached,
            cache_creation: creation,
            output,
            reasoning,
        })
    }

    /// Provider categories with a non-zero count that no price can cover.
    pub fn unpriced_categories(&self) -> Vec<&str> {
        self.other_tokens
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Fold a (possibly repeated) usage report into `self`, keeping the field-wise
    /// maximum. Providers report cumulative counts; taking the maximum never
    /// undercounts when a report is duplicated or arrives out of order.
    pub fn merge_max(&mut self, other: &Usage) {
        fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            }
        }
        self.prompt_tokens = self.prompt_tokens.max(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.max(other.completion_tokens);
        self.total_tokens = self.total_tokens.max(other.total_tokens);
        self.cached_tokens = max_opt(self.cached_tokens, other.cached_tokens);
        self.cache_creation_tokens =
            max_opt(self.cache_creation_tokens, other.cache_creation_tokens);
        self.reasoning_tokens = max_opt(self.reasoning_tokens, other.reasoning_tokens);
        for (k, v) in &other.other_tokens {
            let e = self.other_tokens.entry(k.clone()).or_default();
            *e = (*e).max(*v);
        }
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
    /// Budget / cost accounting for this physical attempt.
    #[serde(default)]
    pub accounting: CostAccounting,
}

impl RequestUsage {
    /// Amount this row counts against budgets: the charged amount, else the actual
    /// cost (rows written before budget accounting existed), else zero.
    pub fn budget_charge(&self) -> Decimal {
        self.accounting
            .charged_cost
            .or_else(|| self.cost.as_ref().map(|c| c.amount))
            .unwrap_or(Decimal::ZERO)
    }
}

/// Financial outcome of one physical attempt (persisted with its row).
///
/// ```text
/// Pending ──► Actual | UsageUnavailable | PricingUnavailable | NotCharged | Abandoned
/// Rejected (never dispatched)
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostStatus {
    /// Not determined (rows from before budget accounting).
    #[default]
    Unknown,
    /// In flight: the worst-case estimate is reserved and charged. A row still
    /// `Pending` after a restart is an orphaned reservation and stays charged.
    Pending,
    /// Actual usage × known pricing.
    Actual,
    /// No (final) usage: the reservation (or, without a budget, the estimate when
    /// one exists) is charged.
    UsageUnavailable,
    /// Usage known but not priceable (no price, missing rate for a used class,
    /// unknown category, inconsistent counts); charged like `UsageUnavailable`.
    PricingUnavailable,
    /// Failed with a definitive error status before generating; nothing charged.
    NotCharged,
    /// Blocked before dispatch (budget / pricing gate); nothing sent, nothing charged.
    Rejected,
    /// The caller cancelled or dropped the request / stream before settlement:
    /// charged the actual cost when final usage had arrived, otherwise the
    /// reservation (or estimate).
    Abandoned,
    /// Settled earlier, then corrected to what the provider reported or billed
    /// ([`crate::AiClient::reconcile_attempt`]); `cost_note` records the source,
    /// the previous status and charge, and the adjustment.
    Reconciled,
}

/// Budget accounting attached to each [`RequestUsage`] row (one row per physical
/// attempt: every retry and every fallback is its own row).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostAccounting {
    /// How the cost was determined.
    pub status: CostStatus,
    /// Pre-flight worst-case estimate (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<Decimal>,
    /// Amount reserved in the spend ledger before dispatch (USD); `None` when the
    /// attempt was not budget-controlled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_cost: Option<Decimal>,
    /// Amount counted against budgets (USD): actual cost when known, the
    /// reservation / estimate when cost is unknown, zero when nothing was consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charged_cost: Option<Decimal>,
    /// Extra budget scope this spend counts toward (e.g. `agent:<id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_scope: Option<String>,
    /// Logical request this attempt belongs to (shared across retries / fallbacks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_request_id: Option<RequestId>,
    /// 1-based physical attempt number within the logical request.
    #[serde(default)]
    pub attempt: u32,
    /// 0 for the first try on a provider, n for the n-th retry on the same provider.
    #[serde(default)]
    pub retry: u32,
    /// Whether the HTTP request was handed to the provider adapter.
    #[serde(default)]
    pub dispatched: bool,
    /// Why the budget gate rejected the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
    /// Why the cost is not `Actual` (missing usage, missing rate, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_note: Option<String>,
    /// Classification of the error that ended the attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
    /// Price sheet the attempt was estimated and priced with
    /// ([`crate::pricing::ModelPricing::version`]); the sheet itself is kept by the
    /// storage ([`crate::storage::Storage::get_pricing_version`]), so a later price
    /// change never reprices this attempt. `None` for rows from before P2.3 and for
    /// attempts on a model without a price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_version: Option<String>,
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
    /// Sum of known actual costs (attempts with unknown cost are not in here).
    pub total_cost: Decimal,
    /// Average latency.
    pub average_latency_ms: f64,
    /// Dispatched attempts whose actual cost is unknown (no usage, no price,
    /// abandoned) — counted, never treated as zero.
    #[serde(default)]
    pub unknown_cost_requests: u64,
    /// Sum of [`RequestUsage::budget_charge`]: what the budget ledger counts
    /// (actual costs plus reservations kept for unknown costs).
    #[serde(default)]
    pub charged_cost: Decimal,
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
        } else if counts_as_unknown(row) {
            self.unknown_cost_requests += 1;
        }
        self.charged_cost += row.budget_charge();
        let n = self.requests as f64;
        self.average_latency_ms =
            ((self.average_latency_ms * (n - 1.0)) + row.latency_ms as f64) / n;
    }
}

/// Aggregates of [`UsageStatistics`] that can also take a row back out (for
/// reconciliation): latency is kept as a sum instead of a running average.
#[derive(Debug, Clone, Default)]
struct Aggregate {
    stats: UsageStatistics,
    latency_sum_ms: u128,
}

impl Aggregate {
    fn add(&mut self, row: &RequestUsage) {
        self.stats.record(row);
        self.latency_sum_ms += u128::from(row.latency_ms);
        self.refresh_latency();
    }

    /// Exact inverse of [`Aggregate::add`] for the same row.
    fn remove(&mut self, row: &RequestUsage) {
        let s = &mut self.stats;
        s.requests = s.requests.saturating_sub(1);
        if row.success {
            s.successful_requests = s.successful_requests.saturating_sub(1);
        } else {
            s.failed_requests = s.failed_requests.saturating_sub(1);
        }
        s.input_tokens = s.input_tokens.saturating_sub(row.usage.prompt_tokens);
        s.output_tokens = s.output_tokens.saturating_sub(row.usage.completion_tokens);
        s.total_tokens = s.total_tokens.saturating_sub(row.usage.total_tokens);
        if let Some(cost) = &row.cost {
            s.total_cost -= cost.amount;
        } else if counts_as_unknown(row) {
            s.unknown_cost_requests = s.unknown_cost_requests.saturating_sub(1);
        }
        s.charged_cost -= row.budget_charge();
        self.latency_sum_ms = self
            .latency_sum_ms
            .saturating_sub(u128::from(row.latency_ms));
        self.refresh_latency();
    }

    fn refresh_latency(&mut self) {
        self.stats.average_latency_ms = if self.stats.requests == 0 {
            0.0
        } else {
            self.latency_sum_ms as f64 / self.stats.requests as f64
        };
    }
}

fn counts_as_unknown(row: &RequestUsage) -> bool {
    row.success
        || matches!(
            row.accounting.status,
            CostStatus::UsageUnavailable | CostStatus::PricingUnavailable | CostStatus::Abandoned
        )
}

/// Default number of recent attempt rows kept in memory.
pub const DEFAULT_RECENT_ATTEMPTS: usize = 10_000;
/// Days of per-day statistics kept in memory.
const DAYS_KEPT: usize = 400;

#[derive(Debug)]
struct UsageState {
    capacity: usize,
    total: Aggregate,
    by_day: BTreeMap<chrono::NaiveDate, Aggregate>,
    by_provider: HashMap<ProviderId, Aggregate>,
    /// Recent rows (content stripped), oldest first in `order`.
    rows: HashMap<RequestId, RequestUsage>,
    order: VecDeque<RequestId>,
    /// Logical request id → its attempts among the recent rows.
    by_logical: HashMap<RequestId, Vec<RequestId>>,
}

impl UsageState {
    fn aggregates(&mut self, row: &RequestUsage) -> [&mut Aggregate; 3] {
        let day = self.by_day.entry(row.started_at.date_naive()).or_default();
        let provider = self.by_provider.entry(row.provider.clone()).or_default();
        [&mut self.total, day, provider]
    }

    fn evict(&mut self) {
        while self.rows.len() > self.capacity {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            if let Some(row) = self.rows.remove(&id) {
                let logical = logical_id(&row);
                if let Some(ids) = self.by_logical.get_mut(&logical) {
                    ids.retain(|i| i != &id);
                    if ids.is_empty() {
                        self.by_logical.remove(&logical);
                    }
                }
            }
        }
        while self.by_day.len() > DAYS_KEPT {
            self.by_day.pop_first();
        }
    }
}

fn logical_id(row: &RequestUsage) -> RequestId {
    row.accounting.logical_request_id.unwrap_or(row.request_id)
}

/// Rows are kept without prompt / response content (it stays in storage).
fn strip_content(mut row: RequestUsage) -> RequestUsage {
    row.request_json = serde_json::json!({});
    row.response_json = None;
    row
}

/// In-memory usage statistics of this process plus a bounded index of its
/// recent attempt rows.
///
/// * Statistics (all-time, per UTC day, per provider) are updated incrementally:
///   O(1) per attempt, no row is rescanned to answer a query.
/// * Only the most recent attempts (`DEFAULT_RECENT_ATTEMPTS` by default) are
///   kept as rows, **without** prompt / response content; older rows, content
///   and other processes' rows are in [`crate::Storage`].
/// * The budget ledger, not this manager, is the source of truth for spend.
#[derive(Debug)]
pub struct UsageManager {
    state: std::sync::Mutex<UsageState>,
}

impl Default for UsageManager {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RECENT_ATTEMPTS)
    }
}

impl UsageManager {
    /// Create empty manager keeping [`DEFAULT_RECENT_ATTEMPTS`] recent rows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create empty manager keeping at most `capacity` recent rows.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(UsageState {
                capacity,
                total: Aggregate::default(),
                by_day: BTreeMap::new(),
                by_provider: HashMap::new(),
                rows: HashMap::new(),
                order: VecDeque::new(),
                by_logical: HashMap::new(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, UsageState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a settled attempt. Recording the same attempt id again replaces
    /// the earlier row (its statistics are taken back out first).
    pub fn record(&self, row: RequestUsage) {
        let row = strip_content(row);
        let mut st = self.state();
        if let Some(old) = st.rows.remove(&row.request_id) {
            for agg in st.aggregates(&old) {
                agg.remove(&old);
            }
            st.order.retain(|id| id != &row.request_id);
        }
        for agg in st.aggregates(&row) {
            agg.add(&row);
        }
        let logical = logical_id(&row);
        let ids = st.by_logical.entry(logical).or_default();
        if !ids.contains(&row.request_id) {
            ids.push(row.request_id);
        }
        st.order.push_back(row.request_id);
        st.rows.insert(row.request_id, row);
        st.evict();
    }

    /// Replace the recent row of a corrected attempt (reconciliation), moving
    /// the statistics from `old` to `new`. Returns `false` (and changes nothing)
    /// when the attempt is not among the recent rows of this process.
    pub fn replace(&self, new: RequestUsage) -> bool {
        let new = strip_content(new);
        let mut st = self.state();
        let Some(old) = st.rows.get(&new.request_id).cloned() else {
            return false;
        };
        for agg in st.aggregates(&old) {
            agg.remove(&old);
        }
        for agg in st.aggregates(&new) {
            agg.add(&new);
        }
        st.rows.insert(new.request_id, new);
        true
    }

    /// Recent rows, oldest first (bounded; content stripped).
    pub fn list(&self) -> Vec<RequestUsage> {
        let st = self.state();
        st.order
            .iter()
            .filter_map(|id| st.rows.get(id).cloned())
            .collect()
    }

    /// Statistics of every attempt recorded by this process.
    pub fn statistics(&self) -> UsageStatistics {
        self.state().total.stats.clone()
    }

    /// Recent row of one attempt id.
    pub fn get(&self, id: &RequestId) -> Option<RequestUsage> {
        self.state().rows.get(id).cloned()
    }

    /// Recent rows of every attempt of the logical request `logical`, in
    /// attempt order.
    pub fn attempts_of(&self, logical: &RequestId) -> Vec<RequestUsage> {
        let st = self.state();
        let mut rows: Vec<RequestUsage> = st
            .by_logical
            .get(logical)
            .into_iter()
            .flatten()
            .filter_map(|id| st.rows.get(id).cloned())
            .collect();
        rows.sort_by_key(|r| r.accounting.attempt);
        rows
    }

    /// Stats for a calendar day (UTC; the last 400 days are kept).
    pub fn statistics_for_day(&self, day: chrono::NaiveDate) -> UsageStatistics {
        self.state()
            .by_day
            .get(&day)
            .map(|a| a.stats.clone())
            .unwrap_or_default()
    }

    /// Stats of one provider.
    pub fn statistics_by_provider(&self, provider: &ProviderId) -> UsageStatistics {
        self.state()
            .by_provider
            .get(provider)
            .map(|a| a.stats.clone())
            .unwrap_or_default()
    }
}

/// What a settled attempt is reconciled with (see
/// [`crate::AiClient::reconcile_attempt`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Reconciliation {
    /// Usage the provider reported later (usage API, export), priced with the
    /// pricing registry.
    Usage {
        /// Reported usage of this attempt.
        usage: Usage,
        /// Where it comes from (kept in the audit note).
        source: String,
    },
    /// Amount the provider billed for this attempt (invoice, costs API), USD.
    Amount {
        /// Billed amount (≥ 0).
        amount: Decimal,
        /// Where it comes from (kept in the audit note).
        source: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(logical: RequestId, attempt: u32, charge: i64, day: u32) -> RequestUsage {
        let at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, day, 12, 0, 0).unwrap();
        RequestUsage {
            request_id: if attempt == 1 {
                logical
            } else {
                RequestId::new()
            },
            provider: ProviderId::openai(),
            account: AccountId::new("a"),
            api_key: None,
            model: ModelId::new("m"),
            started_at: at,
            finished_at: at,
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            },
            cost: None,
            success: false,
            latency_ms: 10 * u64::from(attempt),
            request_json: serde_json::json!({ "messages": "secret prompt" }),
            response_json: Some(serde_json::json!({ "text": "answer" })),
            importance: None,
            accounting: CostAccounting {
                status: CostStatus::UsageUnavailable,
                charged_cost: Some(Decimal::from(charge)),
                logical_request_id: Some(logical),
                attempt,
                ..Default::default()
            },
        }
    }

    #[test]
    fn recent_rows_are_bounded_but_statistics_count_everything() {
        let m = UsageManager::with_capacity(3);
        let first = RequestId::new();
        m.record(attempt(first, 1, 1, 1));
        for _ in 0..9 {
            m.record(attempt(RequestId::new(), 1, 1, 2));
        }
        assert_eq!(m.list().len(), 3, "bounded");
        assert!(m.get(&first).is_none(), "oldest evicted");
        assert!(m.attempts_of(&first).is_empty(), "index evicted too");
        let all = m.statistics();
        assert_eq!(all.requests, 10);
        assert_eq!(all.charged_cost, Decimal::from(10));
        assert_eq!(all.unknown_cost_requests, 10);
        assert_eq!(m.statistics_for_day(Utc::now().date_naive()).requests, 0);
        let day1 = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert_eq!(m.statistics_for_day(day1).requests, 1);
        assert_eq!(m.statistics_by_provider(&ProviderId::openai()).requests, 10);
    }

    #[test]
    fn rows_are_kept_without_content() {
        let m = UsageManager::new();
        let id = RequestId::new();
        m.record(attempt(id, 1, 1, 1));
        let row = m.get(&id).unwrap();
        assert_eq!(row.request_json, serde_json::json!({}));
        assert!(row.response_json.is_none());
    }

    #[test]
    fn attempts_of_a_logical_request_in_attempt_order() {
        let m = UsageManager::new();
        let logical = RequestId::new();
        let (a1, a2, a3) = (
            attempt(logical, 1, 1, 1),
            attempt(logical, 2, 2, 1),
            attempt(logical, 3, 3, 1),
        );
        for r in [a3.clone(), a1, a2] {
            m.record(r);
        }
        m.record(attempt(RequestId::new(), 1, 9, 1));
        let rows = m.attempts_of(&logical);
        assert_eq!(
            rows.iter()
                .map(|r| r.accounting.attempt)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(rows[2].request_id, a3.request_id);
    }

    #[test]
    fn replace_moves_statistics_exactly() {
        let m = UsageManager::new();
        let id = RequestId::new();
        let row = attempt(id, 1, 5, 1);
        m.record(row.clone());
        m.record(attempt(RequestId::new(), 2, 1, 1));
        let before = m.statistics();

        let mut fixed = row.clone();
        fixed.accounting.status = CostStatus::Reconciled;
        fixed.accounting.charged_cost = Some(Decimal::from(2));
        fixed.cost = Some(crate::cost::Cost {
            amount: Decimal::from(2),
            ..crate::cost::Cost::zero()
        });
        assert!(m.replace(fixed.clone()));
        let after = m.statistics();
        assert_eq!(after.requests, before.requests);
        assert_eq!(after.charged_cost, before.charged_cost - Decimal::from(3));
        assert_eq!(after.total_cost, before.total_cost + Decimal::from(2));
        assert_eq!(
            after.unknown_cost_requests,
            before.unknown_cost_requests - 1
        );
        assert_eq!(after.average_latency_ms, before.average_latency_ms);
        // Recording the same attempt again replaces, never double-counts.
        m.record(fixed);
        assert_eq!(m.statistics().requests, before.requests);
        assert!(
            !m.replace(attempt(RequestId::new(), 1, 1, 1)),
            "unknown row"
        );
    }

    #[test]
    fn many_attempts_stay_within_capacity() {
        let m = UsageManager::with_capacity(1_000);
        for _ in 0..50_000 {
            m.record(attempt(RequestId::new(), 1, 1, 3));
        }
        assert_eq!(m.list().len(), 1_000);
        assert_eq!(m.statistics().requests, 50_000);
        assert_eq!(m.statistics().charged_cost, Decimal::from(50_000));
    }

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
                ..Default::default()
            },
            cost: Some(Cost {
                amount: Decimal::new(12, 3),
                input_cost: Decimal::new(4, 3),
                output_cost: Decimal::new(8, 3),
                ..Cost::zero()
            }),
            success: true,
            latency_ms: 100,
            request_json: serde_json::json!({}),
            response_json: None,
            importance: None,
            accounting: CostAccounting::default(),
        };
        stats.record(&row);
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.input_tokens, 10);
        assert_eq!(stats.total_cost, Decimal::new(12, 3));
    }

    #[test]
    fn breakdown_is_disjoint_and_rejects_inconsistent_counts() {
        let u = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: Some(30),
            cache_creation_tokens: Some(20),
            reasoning_tokens: Some(10),
            ..Default::default()
        };
        let b = u.breakdown().unwrap();
        assert_eq!((b.input, b.cached_input, b.cache_creation), (50, 30, 20));
        assert_eq!((b.output, b.reasoning), (40, 10));

        let bad = Usage {
            prompt_tokens: 10,
            cached_tokens: Some(11),
            ..Default::default()
        };
        assert!(bad.breakdown().is_err());
        let bad = Usage {
            completion_tokens: 1,
            reasoning_tokens: Some(2),
            ..Default::default()
        };
        assert!(bad.breakdown().is_err());
    }

    #[test]
    fn merge_max_never_undercounts() {
        let mut a = Usage {
            prompt_tokens: 10,
            completion_tokens: 3,
            ..Default::default()
        };
        a.merge_max(&Usage {
            prompt_tokens: 0,
            completion_tokens: 7,
            reasoning_tokens: Some(2),
            ..Default::default()
        });
        assert_eq!((a.prompt_tokens, a.completion_tokens), (10, 7));
        assert_eq!(a.reasoning_tokens, Some(2));
    }
}
