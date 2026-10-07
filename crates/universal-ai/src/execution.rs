//! Financial lifecycle of one physical attempt.
//!
//! Every HTTP request sent to a provider is one *physical attempt* with its own
//! accounting row, reservation and settlement — retries and fallbacks included.
//! The internal `AttemptMeter` is created after the budget gate reserved the worst case
//! and is consumed by exactly one settlement:
//!
//! ```text
//! Reserved ──dispatch──► Dispatched ──► Settled (Actual | UsageUnavailable | PricingUnavailable)
//!                            │  stream: Streaming ──► UsageReceived ──► Settled
//!                            ├─ error ──► Failed (NotCharged | reservation kept)
//!                            └─ dropped / cancelled ──► Abandoned (actual if final usage, else reservation)
//! ```
//!
//! Settlement methods take the meter by value and `Drop` settles a meter that was
//! never settled, so an attempt is settled exactly once. The settlement itself
//! runs on its own task, so cancelling the caller mid-settlement cannot interrupt
//! it. A crash before settlement leaves the persisted `Pending` row charged at its
//! reservation.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::StreamExt;
use rust_decimal::Decimal;
use tracing::Instrument;

use crate::account::ApiKeyManager;
use crate::budget::SpendLedger;
use crate::cost::{cost_from_pricing, CostManager, PricingGap};
use crate::error::{AiError, AiResult};
use crate::events::{AiEvent, EventBus, RequestCompleted};
use crate::pricing::ModelPricing;
use crate::storage::Storage;
use crate::telemetry::{AttemptReport, TelemetrySink};
use crate::types::{ChatResponse, ChatStream, ModelId, ProviderId, StreamEvent};
use crate::usage::{CostStatus, RequestUsage, Usage, UsageManager};

/// Shared services needed to settle attempts (also from spawned tasks).
pub(crate) struct Core {
    pub ledger: Arc<SpendLedger>,
    pub keys: Arc<ApiKeyManager>,
    pub usage: Arc<UsageManager>,
    pub cost: Arc<CostManager>,
    pub events: Arc<EventBus>,
    pub telemetry: Arc<dyn TelemetrySink>,
    /// Persist prompt / response content in request rows.
    pub store_content: bool,
    /// Models whose worst case was falsified by a bill above the reserved token
    /// bounds (see [`AiError::WorstCaseUnbounded`]).
    pub unbounded: Mutex<HashSet<(ProviderId, ModelId)>>,
    /// Persists the price sheets attempts are priced with.
    pub pricing_journal: PricingJournal,
}

/// Saves each price sheet to storage before the first row that references its
/// version, once per process.
pub(crate) struct PricingJournal {
    storage: Arc<dyn Storage>,
    saved: Mutex<HashSet<String>>,
}

impl PricingJournal {
    pub(crate) fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            saved: Mutex::default(),
        }
    }

    /// Persist `pricing` (unless this process already did) and return its version.
    pub(crate) async fn ensure(&self, pricing: &ModelPricing) -> AiResult<String> {
        let version = pricing.version();
        let known = self
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&version);
        if !known {
            self.storage.save_pricing_version(pricing).await?;
            self.saved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(version.clone());
        }
        Ok(version)
    }
}

impl Core {
    /// Whether an attempt on `provider` / `model` was billed beyond its reserved
    /// token bounds: its worst case can no longer be trusted.
    pub(crate) fn is_unbounded(&self, provider: &ProviderId, model: &ModelId) -> bool {
        self.unbounded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(provider.clone(), model.clone()))
    }

    /// Fail closed when a budget-controlled attempt was billed more tokens than
    /// its reservation assumed possible (input above the byte bound, output above
    /// `max_tokens`): the actual cost is still charged, and the model is marked so
    /// that further budget-controlled requests to it are refused before dispatch.
    fn check_bounds(&self, s: &MeterState) {
        if !s.controlled {
            return;
        }
        let u = &s.row.usage;
        let input_over = u.prompt_tokens > s.input_bound;
        let output_over = s.output_bound.is_some_and(|b| u.completion_tokens > b);
        if !input_over && !output_over {
            return;
        }
        tracing::error!(
            request_id = %s.row.request_id,
            provider = %s.row.provider,
            model = %s.requested_model,
            input_tokens = u.prompt_tokens,
            input_bound = s.input_bound,
            output_tokens = u.completion_tokens,
            output_bound = ?s.output_bound,
            reserved_cost = %s.reserved,
            charged_cost = ?s.row.accounting.charged_cost,
            "provider billed more tokens than the reserved worst case; \
             budget-controlled requests to this model are refused from now on"
        );
        self.unbounded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((s.row.provider.clone(), s.requested_model.clone()));
    }

    /// [`Core::finish_now`] on its own task, awaited: the result is visible when
    /// this returns, but dropping the caller (aborted task, client disconnect,
    /// outer timeout, dropped stream) cannot interrupt a settlement half-way and
    /// leave the attempt `Pending` or missing from the statistics.
    pub(crate) async fn finish(
        self: &Arc<Self>,
        reserved: Decimal,
        row: RequestUsage,
        ttft: Option<Duration>,
    ) {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let core = Arc::clone(self);
                let task = handle.spawn(
                    async move { core.finish_now(reserved, row, ttft).await }
                        .instrument(tracing::Span::current()),
                );
                let _ = task.await;
            }
            Err(_) => self.finish_now(reserved, row, ttft).await,
        }
    }

    /// Persist the final row, move the ledger from `reserved` to the final charge,
    /// attribute it to its key, and report it. Never fails: if persisting fails the
    /// reservation stays charged (in memory and in the persisted `Pending` row).
    async fn finish_now(&self, reserved: Decimal, row: RequestUsage, ttft: Option<Duration>) {
        // The sheet a settled row references is stored before the row.
        if let Some(pricing) = row
            .accounting
            .pricing_version
            .as_deref()
            .and_then(|v| self.cost.pricing().get_version(v))
        {
            if let Err(err) = self.pricing_journal.ensure(&pricing).await {
                tracing::error!(
                    request_id = %row.request_id,
                    pricing_version = ?row.accounting.pricing_version,
                    error = %err,
                    "failed to persist the price sheet of an attempt"
                );
            }
        }
        if let Err(err) = self.ledger.settle(reserved, &row).await {
            tracing::error!(
                request_id = %row.request_id,
                error = %err,
                "failed to persist request accounting; reservation stays charged"
            );
        }
        if let Some(ref kid) = row.api_key {
            self.keys.record_usage(kid, &row).await;
        }
        let report = attempt_report(&row, ttft);
        log_attempt(&report);
        self.telemetry.attempt_finished(&report);
        self.usage.record(row);
    }
}

fn attempt_report(row: &RequestUsage, ttft: Option<Duration>) -> AttemptReport {
    let a = &row.accounting;
    let dispatched = a.dispatched || a.status != CostStatus::Rejected;
    AttemptReport {
        logical_request_id: a.logical_request_id.unwrap_or(row.request_id),
        attempt_id: row.request_id,
        attempt: a.attempt,
        retry: a.retry,
        provider: row.provider.clone(),
        model: row.model.clone(),
        key_id: row.api_key.clone(),
        latency_ms: row.latency_ms,
        time_to_first_token_ms: ttft.map(|d| d.as_millis() as u64),
        usage: (dispatched && row.usage != Usage::default()).then(|| row.usage.clone()),
        estimated_cost: a.estimated_cost,
        reserved_cost: a.reserved_cost,
        released_cost: a
            .reserved_cost
            .map(|r| (r - row.budget_charge()).max(Decimal::ZERO)),
        charged_cost: a.charged_cost,
        actual_cost: row.cost.as_ref().map(|c| c.amount),
        status: a.status,
        error_kind: a.error_kind,
    }
}

fn log_attempt(r: &AttemptReport) {
    let usage = r.usage.clone().unwrap_or_default();
    tracing::info!(
        target: "universal_ai::accounting",
        logical_request_id = %r.logical_request_id,
        attempt_id = %r.attempt_id,
        attempt = r.attempt,
        retry = r.retry,
        provider = %r.provider,
        model = %r.model,
        key_id = ?r.key_id.as_ref().map(|k| k.to_string()),
        latency_ms = r.latency_ms,
        ttft_ms = ?r.time_to_first_token_ms,
        input_tokens = usage.prompt_tokens,
        output_tokens = usage.completion_tokens,
        cached_tokens = ?usage.cached_tokens,
        reasoning_tokens = ?usage.reasoning_tokens,
        estimated_cost = ?r.estimated_cost,
        reserved_cost = ?r.reserved_cost,
        released_cost = ?r.released_cost,
        charged_cost = ?r.charged_cost,
        actual_cost = ?r.actual_cost,
        cost_status = ?r.status,
        error_kind = ?r.error_kind,
        "attempt settled"
    );
}

/// Internal lifecycle phase (the persisted outcome is [`CostStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptPhase {
    Reserved,
    Dispatched,
    Streaming,
    UsageReceived,
}

/// How the attempt ended (input to [`settle_row`]).
pub(crate) enum Outcome<'a> {
    /// A complete non-streaming response.
    Response {
        usage: Option<&'a Usage>,
        response_model: &'a ModelId,
    },
    /// The provider call failed.
    Failed(&'a AiError),
    /// The stream ended (`failure` = the error that ended it).
    StreamEnded {
        completed: bool,
        failure: Option<&'a AiError>,
    },
    /// Dropped or cancelled before settlement.
    Abandoned,
}

struct MeterState {
    row: RequestUsage,
    /// Model id of the request (the provider may answer with a dated variant).
    requested_model: ModelId,
    /// Price sheet of the requested model the estimate / reservation used; the
    /// attempt settles with it even if the registry changes mid-flight.
    pricing: Option<ModelPricing>,
    reserved: Decimal,
    controlled: bool,
    started: Instant,
    /// Input token bound the reservation assumed.
    input_bound: u64,
    /// Output token bound the reservation assumed (sent as `max_tokens` when
    /// budget-controlled).
    output_bound: Option<u64>,
    phase: AttemptPhase,
    usage: Option<Usage>,
    first_token: Option<Duration>,
    completed: bool,
}

/// Owns one attempt's accounting from reservation to settlement.
pub(crate) struct AttemptMeter {
    core: Arc<Core>,
    state: Option<MeterState>,
}

impl AttemptMeter {
    pub(crate) fn new(
        core: Arc<Core>,
        row: RequestUsage,
        reserved: Decimal,
        controlled: bool,
        started: Instant,
        (input_bound, output_bound): (u64, Option<u64>),
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            core,
            state: Some(MeterState {
                requested_model: row.model.clone(),
                pricing,
                row,
                reserved,
                controlled,
                started,
                input_bound,
                output_bound,
                phase: AttemptPhase::Reserved,
                usage: None,
                first_token: None,
                completed: false,
            }),
        }
    }

    /// The request is being handed to the provider adapter.
    pub(crate) fn dispatched(&mut self) {
        if let Some(s) = self.state.as_mut() {
            s.phase = AttemptPhase::Dispatched;
            s.row.accounting.dispatched = true;
        }
    }

    /// Settle a complete response; fills `response.cost` and `request_id`.
    pub(crate) async fn settle_response(mut self, response: &mut ChatResponse) -> CostStatus {
        let Some(mut s) = self.state.take() else {
            return CostStatus::Unknown;
        };
        response.request_id = s.row.request_id;
        s.row.model = response.model.clone();
        s.row.success = true;
        s.row.response_json = Some(if self.core.store_content {
            serde_json::to_value(&*response).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({ "content": "not stored" })
        });
        let usage = response.usage.clone();
        settle_row(
            &mut s,
            &self.core.cost,
            Outcome::Response {
                usage: usage.as_ref(),
                response_model: &response.model,
            },
        );
        self.core.check_bounds(&s);
        response.cost = s.row.cost.clone();
        let status = s.row.accounting.status;
        self.emit_completed(&s);
        self.core.finish(s.reserved, s.row, None).await;
        status
    }

    /// Settle a failed provider call.
    pub(crate) async fn settle_error(mut self, err: &AiError) {
        if let Some(mut s) = self.state.take() {
            settle_row(&mut s, &self.core.cost, Outcome::Failed(err));
            self.core.finish(s.reserved, s.row, None).await;
        }
    }

    /// Settle a cancellation observed by the caller's cancel signal.
    pub(crate) async fn settle_abandoned(mut self) {
        if let Some(mut s) = self.state.take() {
            settle_row(&mut s, &self.core.cost, Outcome::Abandoned);
            self.core.check_bounds(&s);
            self.core.finish(s.reserved, s.row, s.first_token).await;
        }
    }

    /// Track a streamed event (usage, first token, completion).
    pub(crate) fn observe(&mut self, event: &StreamEvent) {
        let Some(s) = self.state.as_mut() else {
            return;
        };
        if s.phase == AttemptPhase::Dispatched {
            s.phase = AttemptPhase::Streaming;
        }
        match event {
            StreamEvent::TextDelta { .. } | StreamEvent::ToolCall { .. } => {
                if s.first_token.is_none() {
                    s.first_token = Some(s.started.elapsed());
                }
            }
            StreamEvent::Usage { usage } => {
                // Usage events are final counts; duplicates keep the maximum.
                s.usage.get_or_insert_with(Usage::default).merge_max(usage);
                s.phase = AttemptPhase::UsageReceived;
            }
            StreamEvent::Done => s.completed = true,
        }
        if let StreamEvent::TextDelta { text } = event {
            if let Some(serde_json::Value::String(buf)) =
                s.row.response_json.as_mut().and_then(|v| v.get_mut("text"))
            {
                buf.push_str(text);
            }
        }
    }

    /// Settle when the stream ended (normally or with `failure`).
    pub(crate) async fn settle_stream(mut self, failure: Option<&AiError>) -> CostStatus {
        let Some(mut s) = self.state.take() else {
            return CostStatus::Unknown;
        };
        let completed = s.completed;
        s.row.success = completed && failure.is_none();
        settle_row(
            &mut s,
            &self.core.cost,
            Outcome::StreamEnded { completed, failure },
        );
        self.core.check_bounds(&s);
        let status = s.row.accounting.status;
        if s.row.success {
            self.emit_completed(&s);
        }
        self.core.finish(s.reserved, s.row, s.first_token).await;
        status
    }

    fn emit_completed(&self, s: &MeterState) {
        let row = &s.row;
        self.core
            .events
            .emit(AiEvent::RequestCompleted(RequestCompleted {
                request_id: row.request_id,
                provider: row.provider.clone(),
                model: row.model.clone(),
                usage: (row.usage != Usage::default()).then(|| row.usage.clone()),
                latency_ms: row.latency_ms,
                at: row.finished_at,
            }));
        self.core.telemetry.request_completed(
            &row.request_id,
            &row.provider,
            &row.model,
            (row.usage != Usage::default()).then_some(&row.usage),
            row.latency_ms,
        );
    }
}

impl Drop for AttemptMeter {
    fn drop(&mut self) {
        let Some(mut s) = self.state.take() else {
            return;
        };
        settle_row(&mut s, &self.core.cost, Outcome::Abandoned);
        self.core.check_bounds(&s);
        tracing::warn!(
            request_id = %s.row.request_id,
            provider = %s.row.provider,
            charged_cost = ?s.row.accounting.charged_cost,
            "attempt abandoned before settlement (request future or stream dropped)"
        );
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let core = Arc::clone(&self.core);
                handle.spawn(async move {
                    core.finish_now(s.reserved, s.row, s.first_token).await;
                });
            }
            // No runtime (dropped during shutdown): the persisted `Pending` row
            // keeps the reservation charged — the same outcome as a crash.
            Err(_) => self.core.usage.record(s.row),
        }
    }
}

/// Final financial fields of an attempt. Unknown cost is never zero: when the
/// actual cost cannot be determined, a budget-controlled attempt is charged its
/// reservation and an uncontrolled one its estimate (when one exists).
fn settle_row(s: &mut MeterState, cost: &CostManager, outcome: Outcome<'_>) {
    s.row.finished_at = Utc::now();
    s.row.latency_ms = s.started.elapsed().as_millis() as u64;
    let unknown_charge = if s.controlled {
        Some(s.reserved)
    } else {
        s.row.accounting.estimated_cost
    };
    match outcome {
        Outcome::Response {
            usage,
            response_model,
        } => match usage {
            None => mark_unknown(
                &mut s.row,
                CostStatus::UsageUnavailable,
                unknown_charge,
                "provider returned no usage",
            ),
            Some(u) => price_into(
                s,
                cost,
                u,
                response_model,
                CostStatus::Actual,
                unknown_charge,
            ),
        },
        Outcome::Failed(err) => {
            s.row.success = false;
            s.row.accounting.error_kind = Some(err.kind());
            if err.may_have_consumed_tokens() {
                let note = format!(
                    "{:?} error after dispatch: tokens may have been consumed",
                    err.kind()
                );
                mark_unknown(
                    &mut s.row,
                    CostStatus::UsageUnavailable,
                    unknown_charge,
                    &note,
                );
            } else {
                s.row.accounting.status = CostStatus::NotCharged;
                s.row.accounting.charged_cost = Some(Decimal::ZERO);
            }
        }
        Outcome::StreamEnded { completed, failure } => {
            if let Some(err) = failure {
                s.row.accounting.error_kind = Some(err.kind());
            }
            match (s.usage.clone(), failure) {
                (Some(u), _) => {
                    let model = s.row.model.clone();
                    price_into(s, cost, &u, &model, CostStatus::Actual, unknown_charge);
                }
                (None, Some(err)) if !err.may_have_consumed_tokens() => {
                    s.row.accounting.status = CostStatus::NotCharged;
                    s.row.accounting.charged_cost = Some(Decimal::ZERO);
                }
                (None, _) => mark_unknown(
                    &mut s.row,
                    CostStatus::UsageUnavailable,
                    unknown_charge,
                    if completed {
                        "stream completed without usage"
                    } else {
                        "stream ended before completion without usage"
                    },
                ),
            }
        }
        Outcome::Abandoned => {
            s.row.success = false;
            s.row.accounting.error_kind = Some(crate::error::ErrorKind::Cancellation);
            if !s.row.accounting.dispatched {
                s.row.accounting.status = CostStatus::Abandoned;
                s.row.accounting.charged_cost = Some(Decimal::ZERO);
                s.row.accounting.cost_note = Some("cancelled before dispatch".into());
            } else if let Some(u) = s.usage.clone() {
                let model = s.row.model.clone();
                price_into(s, cost, &u, &model, CostStatus::Abandoned, unknown_charge);
            } else {
                mark_unknown(
                    &mut s.row,
                    CostStatus::Abandoned,
                    unknown_charge,
                    "cancelled after dispatch without final usage",
                );
            }
        }
    }
}

fn mark_unknown(row: &mut RequestUsage, status: CostStatus, charge: Option<Decimal>, note: &str) {
    row.cost = None;
    row.accounting.status = status;
    row.accounting.charged_cost = charge;
    row.accounting.cost_note = Some(note.to_string());
}

/// Price `usage` and record the result; `known_status` is used when the cost is
/// known. Price sheets, in order: the provider-reported model's current sheet
/// (when it answered with another model id, e.g. a dated variant), then the
/// requested model's sheet pinned at reservation (its current sheet when none
/// was pinned). The sheet that priced the usage is recorded on the row.
fn price_into(
    s: &mut MeterState,
    cost: &CostManager,
    usage: &Usage,
    model: &ModelId,
    known_status: CostStatus,
    unknown_charge: Option<Decimal>,
) {
    s.row.usage = usage.clone();
    let registry = cost.pricing();
    let mut sheets = Vec::with_capacity(2);
    if model != &s.requested_model {
        sheets.extend(registry.get_price_sync(&s.row.provider, model));
    }
    match &s.pricing {
        Some(p) => sheets.push(p.clone()),
        None => sheets.extend(registry.get_price_sync(&s.row.provider, &s.requested_model)),
    }
    let mut gap = None;
    for sheet in &sheets {
        match cost_from_pricing(sheet, usage) {
            Ok(c) => {
                s.row.accounting.status = known_status;
                s.row.accounting.charged_cost = Some(c.amount);
                s.row.accounting.cost_note = None;
                s.row.accounting.pricing_version = Some(sheet.version());
                s.row.cost = Some(c);
                return;
            }
            Err(g) => {
                gap.get_or_insert(g);
            }
        }
    }
    let gap = gap.unwrap_or(PricingGap::NoPrice);
    let status = if known_status == CostStatus::Abandoned {
        CostStatus::Abandoned
    } else {
        CostStatus::PricingUnavailable
    };
    mark_unknown(&mut s.row, status, unknown_charge, &gap.to_string());
}

/// Forward a provider stream while metering it. Pull-based: no task is spawned,
/// so dropping the returned stream drops the provider stream (closing the
/// connection) and settles the attempt as abandoned. The stream is settled before
/// it reports its end to the consumer.
pub(crate) fn metered_stream(
    inner: ChatStream,
    mut meter: AttemptMeter,
    reject_missing_usage: bool,
) -> ChatStream {
    let store_content = meter.core.store_content;
    if let Some(s) = meter.state.as_mut() {
        s.row.response_json = Some(if store_content {
            serde_json::json!({ "text": "", "streamed": true })
        } else {
            serde_json::json!({ "content": "not stored", "streamed": true })
        });
    }
    struct Ctx {
        inner: ChatStream,
        meter: Option<AttemptMeter>,
        reject_missing_usage: bool,
        done: bool,
    }
    let ctx = Ctx {
        inner,
        meter: Some(meter),
        reject_missing_usage,
        done: false,
    };
    Box::pin(futures::stream::unfold(ctx, |mut ctx| async move {
        if ctx.done {
            return None;
        }
        match ctx.inner.next().await {
            Some(Ok(event)) => {
                if let Some(m) = ctx.meter.as_mut() {
                    m.observe(&event);
                }
                Some((Ok(event), ctx))
            }
            Some(Err(err)) => {
                if let Some(m) = ctx.meter.take() {
                    m.settle_stream(Some(&err)).await;
                }
                ctx.done = true;
                Some((Err(err), ctx))
            }
            None => {
                let meter = ctx.meter.take()?;
                let (provider, model) = meter
                    .state
                    .as_ref()
                    .map(|s| (s.row.provider.clone(), s.row.model.clone()))?;
                let status = meter.settle_stream(None).await;
                ctx.done = true;
                if ctx.reject_missing_usage && status == CostStatus::UsageUnavailable {
                    return Some((Err(AiError::UsageUnavailable { provider, model }), ctx));
                }
                None
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{ModelPricing, PricingRegistry};
    use crate::types::{AccountId, ProviderId, RequestId};
    use crate::usage::CostAccounting;

    fn state(controlled: bool, dispatched: bool) -> MeterState {
        MeterState {
            requested_model: ModelId::new("m"),
            pricing: None,
            row: RequestUsage {
                request_id: RequestId::new(),
                provider: ProviderId::openai(),
                account: AccountId::new("a"),
                api_key: None,
                model: ModelId::new("m"),
                started_at: Utc::now(),
                finished_at: Utc::now(),
                usage: Usage::default(),
                cost: None,
                success: false,
                latency_ms: 0,
                request_json: serde_json::json!({}),
                response_json: None,
                importance: None,
                accounting: CostAccounting {
                    estimated_cost: Some(Decimal::from(5)),
                    dispatched,
                    ..Default::default()
                },
            },
            reserved: if controlled {
                Decimal::from(5)
            } else {
                Decimal::ZERO
            },
            controlled,
            started: Instant::now(),
            input_bound: 18,
            output_bound: Some(100),
            phase: AttemptPhase::Dispatched,
            usage: None,
            first_token: None,
            completed: false,
        }
    }

    fn cost() -> CostManager {
        let reg = PricingRegistry::new();
        reg.upsert(ModelPricing::per_million(
            ProviderId::openai(),
            "m",
            Decimal::from(1_000_000),
            Decimal::from(1_000_000),
        ));
        CostManager::new(reg)
    }

    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: p + c,
            ..Default::default()
        }
    }

    #[test]
    fn unknown_cost_is_never_zero() {
        let cost = cost();
        let model = ModelId::new("m");
        let timeout = AiError::Timeout;
        let outcomes: Vec<Outcome<'_>> = vec![
            Outcome::Response {
                usage: None,
                response_model: &model,
            },
            Outcome::Failed(&timeout),
            Outcome::StreamEnded {
                completed: false,
                failure: None,
            },
            Outcome::Abandoned,
        ];
        for outcome in outcomes {
            for controlled in [true, false] {
                let mut s = state(controlled, true);
                settle_row(&mut s, &cost, outcome_clone(&outcome));
                assert_eq!(
                    s.row.accounting.charged_cost,
                    Some(Decimal::from(5)),
                    "controlled={controlled}"
                );
                assert!(s.row.accounting.cost_note.is_some());
            }
        }
    }

    fn outcome_clone<'a>(o: &Outcome<'a>) -> Outcome<'a> {
        match o {
            Outcome::Response {
                usage,
                response_model,
            } => Outcome::Response {
                usage: *usage,
                response_model,
            },
            Outcome::Failed(e) => Outcome::Failed(e),
            Outcome::StreamEnded { completed, failure } => Outcome::StreamEnded {
                completed: *completed,
                failure: *failure,
            },
            Outcome::Abandoned => Outcome::Abandoned,
        }
    }

    #[test]
    fn definitive_failure_and_undispatched_cancel_charge_nothing() {
        let cost = cost();
        let mut s = state(true, true);
        let err = AiError::from_http_status(ProviderId::openai(), 400, "bad", None);
        settle_row(&mut s, &cost, Outcome::Failed(&err));
        assert_eq!(s.row.accounting.status, CostStatus::NotCharged);
        assert_eq!(s.row.accounting.charged_cost, Some(Decimal::ZERO));

        let mut s = state(true, false);
        settle_row(&mut s, &cost, Outcome::Abandoned);
        assert_eq!(s.row.accounting.charged_cost, Some(Decimal::ZERO));
    }

    #[test]
    fn final_usage_settles_actual_even_when_abandoned() {
        let cost = cost();
        let mut s = state(true, true);
        s.usage = Some(usage(1, 2));
        settle_row(&mut s, &cost, Outcome::Abandoned);
        assert_eq!(s.row.accounting.status, CostStatus::Abandoned);
        assert_eq!(s.row.accounting.charged_cost, Some(Decimal::from(3)));
        assert_eq!(s.row.cost.as_ref().unwrap().amount, Decimal::from(3));

        let mut s = state(true, true);
        s.usage = Some(usage(1, 2));
        settle_row(
            &mut s,
            &cost,
            Outcome::StreamEnded {
                completed: true,
                failure: None,
            },
        );
        assert_eq!(s.row.accounting.status, CostStatus::Actual);
    }
}
