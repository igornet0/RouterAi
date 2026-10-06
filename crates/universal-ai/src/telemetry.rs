//! Optional telemetry hooks — disabled by default; never send prompt bodies.
//!
//! Every physical attempt also emits a structured `tracing` event
//! (target `universal_ai::accounting`) with the same fields as [`AttemptReport`].

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::ErrorKind;
use crate::types::{KeyId, ModelId, ProviderId, RequestId};
use crate::usage::{CostStatus, Usage};

/// Ids, timings, token counts and money of one settled physical attempt.
/// Contains no prompt / response content and no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReport {
    /// Logical request (shared by retries and fallbacks).
    pub logical_request_id: RequestId,
    /// This physical attempt.
    pub attempt_id: RequestId,
    /// 1-based attempt number within the logical request.
    pub attempt: u32,
    /// Retry number on the same provider (0 = first try).
    pub retry: u32,
    /// Provider.
    pub provider: ProviderId,
    /// Model.
    pub model: ModelId,
    /// API key record id (never the secret).
    pub key_id: Option<KeyId>,
    /// Wall-clock latency.
    pub latency_ms: u64,
    /// Time to first streamed token (streams only).
    pub time_to_first_token_ms: Option<u64>,
    /// Reported usage.
    pub usage: Option<Usage>,
    /// Pre-flight worst-case estimate.
    pub estimated_cost: Option<Decimal>,
    /// Amount reserved before dispatch.
    pub reserved_cost: Option<Decimal>,
    /// Part of the reservation returned to the budget at settlement.
    pub released_cost: Option<Decimal>,
    /// Amount counted against budgets.
    pub charged_cost: Option<Decimal>,
    /// Actual cost (usage × pricing) when known.
    pub actual_cost: Option<Decimal>,
    /// Financial outcome.
    pub status: CostStatus,
    /// Error class when the attempt failed.
    pub error_kind: Option<ErrorKind>,
}

/// Opt-in telemetry sink.
pub trait TelemetrySink: Send + Sync {
    /// Request started (ids only).
    fn request_started(&self, request_id: &RequestId, provider: &ProviderId, model: &ModelId);
    /// Request completed.
    fn request_completed(
        &self,
        request_id: &RequestId,
        provider: &ProviderId,
        model: &ModelId,
        usage: Option<&Usage>,
        latency_ms: u64,
    );
    /// Request failed (safe message only).
    fn request_failed(&self, request_id: &RequestId, message: &str);
    /// One physical attempt was settled (every retry / fallback / abandoned stream).
    fn attempt_finished(&self, report: &AttemptReport) {
        let _ = report;
    }
}

/// No-op sink.
#[derive(Debug, Default)]
pub struct NoopTelemetry;

impl TelemetrySink for NoopTelemetry {
    fn request_started(&self, _: &RequestId, _: &ProviderId, _: &ModelId) {}
    fn request_completed(
        &self,
        _: &RequestId,
        _: &ProviderId,
        _: &ModelId,
        _: Option<&Usage>,
        _: u64,
    ) {
    }
    fn request_failed(&self, _: &RequestId, _: &str) {}
}
