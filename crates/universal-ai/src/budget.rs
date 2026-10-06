//! Fail-closed budget control.
//!
//! ```text
//! worst-case estimate → reserve (atomic check + provisional charge, persisted)
//!   → provider request → actual usage × pricing → settle (replace reservation)
//! ```
//!
//! Spend lives in the same [`Storage`] as request history: every attempt row carries
//! its [`CostAccounting::charged_cost`], so daily / monthly totals are recomputed
//! from storage after a restart. A reservation is charged the moment it is made;
//! settling replaces it with the actual cost. An attempt that never settles
//! (cancelled future, crash) therefore stays charged at its worst case.
//!
//! [`CostAccounting::charged_cost`]: crate::usage::CostAccounting::charged_cost

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::cost::{CostEstimate, CostManager, InputBound, OutputBoundSource};
use crate::error::{AiError, AiResult};
use crate::models::ModelRegistry;
use crate::storage::Storage;
use crate::types::{ChatRequest, ProviderId};
use crate::usage::RequestUsage;

/// Per-message overhead (role markers, separators) added to the input bound.
const MESSAGE_OVERHEAD_TOKENS: u64 = 8;
/// Providers inject tool-use instructions when tools are offered.
const TOOLS_OVERHEAD_TOKENS: u64 = 1024;

/// Extra budget scope for a request (in addition to the global policy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetScope {
    /// Scope name, e.g. `agent:<lineage id>`.
    pub name: String,
    /// Daily limit for this scope (UTC day).
    pub daily_limit: Option<Decimal>,
}

/// Per-request budget options (from [`crate::ChatBuilder`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestBudget {
    /// Hard cap for this request's worst-case cost (e.g. remaining run budget).
    pub max_cost: Option<Decimal>,
    /// Scope the spend also counts toward.
    pub scope: Option<BudgetScope>,
}

/// Spend snapshot for one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetStatus {
    /// `None` = global.
    pub scope: Option<String>,
    /// Current UTC day.
    pub day: NaiveDate,
    /// Committed spend today (settled + reserved).
    pub daily_spent: Decimal,
    /// Current UTC month (`YYYY-MM`).
    pub month: String,
    /// Committed spend this month (settled + reserved).
    pub monthly_spent: Decimal,
}

/// Upper bound on prompt tokens, itemized: byte-level tokenizers never emit more
/// tokens than UTF-8 bytes, plus per-message and tool-instruction overhead.
/// Non-text content parts never reach this point (rejected before dispatch).
pub(crate) fn input_bound(request: &ChatRequest) -> InputBound {
    let mut b = InputBound {
        message_overhead_tokens: MESSAGE_OVERHEAD_TOKENS,
        ..Default::default()
    };
    for m in &request.messages {
        b.message_bytes += m.content.to_plain_text().len() as u64;
        b.message_bytes += m.name.as_deref().map_or(0, str::len) as u64;
        b.message_overhead_tokens += MESSAGE_OVERHEAD_TOKENS;
        b.tool_call_bytes += m.tool_call_id.as_deref().map_or(0, str::len) as u64;
        for call in &m.tool_calls {
            b.tool_call_bytes +=
                (call.id.len() + call.function.name.len() + call.function.arguments.len()) as u64;
        }
    }
    if !request.tools.is_empty() {
        b.tool_schema_bytes = serde_json::to_string(&request.tools).map_or(0, |s| s.len()) as u64;
        b.tool_overhead_tokens = TOOLS_OVERHEAD_TOKENS;
    }
    if let Some(fmt) = &request.response_format {
        b.tool_schema_bytes += serde_json::to_string(fmt).map_or(0, |s| s.len()) as u64;
    }
    for stop in &request.stop {
        b.message_bytes += stop.len() as u64;
    }
    b
}

/// Total input token bound (see [`input_bound`]).
#[cfg(test)]
pub(crate) fn input_token_upper_bound(request: &ChatRequest) -> u64 {
    input_bound(request).total()
}

/// Output token bound for `request` on `provider`: its `max_tokens`, else the
/// registered model's `max_output_tokens`. Reasoning tokens count inside it.
pub(crate) fn output_bound(
    models: &ModelRegistry,
    provider: &ProviderId,
    request: &ChatRequest,
) -> AiResult<(u64, OutputBoundSource)> {
    if let Some(n) = request.max_tokens {
        return Ok((u64::from(n), OutputBoundSource::RequestMaxTokens));
    }
    models
        .get(&format!("{provider}:{}", request.model))
        .or_else(|| models.get(request.model.as_str()))
        .and_then(|m| m.max_output_tokens)
        .map(|n| (n, OutputBoundSource::ModelMaxOutput))
        .ok_or_else(|| AiError::OutputLimitUnknown {
            model: request.model.clone(),
        })
}

/// Worst-case cost of `request` on `provider`: the input bound at the highest
/// priced input-side rate plus the output bound at the highest priced output-side
/// rate (see [`CostEstimate::explain`]).
pub(crate) fn worst_case_cost(
    cost: &CostManager,
    models: &ModelRegistry,
    provider: &ProviderId,
    request: &ChatRequest,
) -> AiResult<CostEstimate> {
    let output = output_bound(models, provider, request)?;
    cost.estimate_bounds(provider, &request.model, input_bound(request), Some(output))
        .ok_or_else(|| AiError::PricingUnavailable {
            provider: provider.clone(),
            model: request.model.clone(),
        })
}

/// Global limits checked inside the reservation.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PeriodLimits {
    pub daily: Option<Decimal>,
    pub monthly: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Period {
    Day(NaiveDate),
    Month(i32, u32),
}

impl Period {
    fn day(at: DateTime<Utc>) -> Self {
        Self::Day(at.date_naive())
    }

    fn month(at: DateTime<Utc>) -> Self {
        Self::Month(at.year(), at.month())
    }

    /// `[start, end)` in UTC.
    fn window(self) -> (DateTime<Utc>, DateTime<Utc>) {
        let midnight = |d: NaiveDate| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap());
        match self {
            Self::Day(d) => (midnight(d), midnight(d.succ_opt().unwrap_or(d))),
            Self::Month(y, m) => {
                let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or_default();
                let next = if m == 12 {
                    NaiveDate::from_ymd_opt(y + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(y, m + 1, 1)
                }
                .unwrap_or(first);
                (midnight(first), midnight(next))
            }
        }
    }
}

/// `None` scope key = global (all rows).
type TotalsKey = (Option<String>, Period);

/// Committed spend per scope and period, backed by [`Storage`].
pub(crate) struct SpendLedger {
    totals: tokio::sync::Mutex<HashMap<TotalsKey, Decimal>>,
    storage: Arc<dyn Storage>,
}

impl SpendLedger {
    pub(crate) fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            totals: tokio::sync::Mutex::new(HashMap::new()),
            storage,
        }
    }

    fn keys(row: &RequestUsage) -> Vec<TotalsKey> {
        let at = row.started_at;
        let mut keys = vec![(None, Period::day(at)), (None, Period::month(at))];
        if let Some(scope) = &row.accounting.budget_scope {
            keys.push((Some(scope.clone()), Period::day(at)));
            keys.push((Some(scope.clone()), Period::month(at)));
        }
        keys
    }

    async fn load(
        &self,
        totals: &mut HashMap<TotalsKey, Decimal>,
        key: &TotalsKey,
    ) -> AiResult<Decimal> {
        if let Some(v) = totals.get(key) {
            return Ok(*v);
        }
        let (start, end) = key.1.window();
        let spent = self
            .storage
            .spend_in_window(key.0.as_deref(), start, end)
            .await?;
        totals.insert(key.clone(), spent);
        Ok(spent)
    }

    /// Atomically check limits and charge `row` (status Pending, `charged_cost` =
    /// worst case). The pending row is persisted before the totals change, so a
    /// reservation survives restarts. Fails closed if storage cannot answer.
    pub(crate) async fn reserve(
        &self,
        row: &RequestUsage,
        limits: PeriodLimits,
        scope_daily: Option<Decimal>,
    ) -> AiResult<()> {
        let amount = row.budget_charge();
        let mut totals = self.totals.lock().await;
        // Drop finished periods; current ones are reloaded from storage when needed.
        let (today, month) = (Period::day(row.started_at), Period::month(row.started_at));
        totals.retain(|(_, p), _| *p == today || *p == month);

        let keys = Self::keys(row);
        for key in &keys {
            self.load(&mut totals, key).await?;
        }
        let spent = |key: &TotalsKey| totals.get(key).copied().unwrap_or_default();
        let scope = row.accounting.budget_scope.clone();
        let mut checks = vec![
            ((None, today), limits.daily, false),
            ((None, month), limits.monthly, true),
        ];
        if let Some(s) = &scope {
            checks.push(((Some(s.clone()), today), scope_daily, false));
        }
        for (key, limit, monthly) in checks {
            let Some(limit) = limit else { continue };
            let already = spent(&key);
            if already + amount > limit {
                let (scope, spent, requested) = (key.0.clone(), already, amount);
                return Err(if monthly {
                    AiError::MonthlyLimitExceeded {
                        scope,
                        spent,
                        requested,
                        limit,
                    }
                } else {
                    AiError::DailyLimitExceeded {
                        scope,
                        spent,
                        requested,
                        limit,
                    }
                });
            }
        }

        self.storage.save_request(row).await?;
        for key in keys {
            *totals.entry(key).or_default() += amount;
        }
        Ok(())
    }

    /// Persist the final version of an attempt row and move the totals from the
    /// previously charged amount (`reserved`, zero if nothing was reserved) to the
    /// row's final charge. If persisting fails, the reservation stays charged.
    pub(crate) async fn settle(&self, reserved: Decimal, row: &RequestUsage) -> AiResult<()> {
        let mut totals = self.totals.lock().await;
        self.storage.save_request(row).await?;
        let delta = row.budget_charge() - reserved;
        if !delta.is_zero() {
            for key in Self::keys(row) {
                // Only adjust periods already loaded; others read the row from storage.
                if let Some(v) = totals.get_mut(&key) {
                    *v += delta;
                }
            }
        }
        Ok(())
    }

    /// Committed spend (settled + reserved) for the current UTC day and month.
    pub(crate) async fn status(&self, scope: Option<&str>) -> AiResult<BudgetStatus> {
        let now = Utc::now();
        let mut totals = self.totals.lock().await;
        let scope_key = scope.map(str::to_string);
        let daily_spent = self
            .load(&mut totals, &(scope_key.clone(), Period::day(now)))
            .await?;
        let monthly_spent = self
            .load(&mut totals, &(scope_key, Period::month(now)))
            .await?;
        Ok(BudgetStatus {
            scope: scope.map(str::to_string),
            day: now.date_naive(),
            daily_spent,
            month: format!("{:04}-{:02}", now.year(), now.month()),
            monthly_spent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn periods_use_utc_boundaries() {
        let at = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();
        let (start, end) = Period::day(at).window();
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 12, 31, 0, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
        let (start, end) = Period::month(at).window();
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn input_bound_never_below_utf8_bytes() {
        let mut req = ChatRequest::simple("m", "Привет, мир");
        req.messages.push(Message::system("x".repeat(100)));
        let bytes = "Привет, мир".len() as u64 + 100;
        assert!(input_token_upper_bound(&req) >= bytes);
    }
}
