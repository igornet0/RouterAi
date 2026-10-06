//! Optional telemetry hooks — disabled by default; never send prompt bodies.

use crate::types::{ModelId, ProviderId, RequestId};
use crate::usage::Usage;

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
