//! Shared HTTP helpers for adapters.

use serde_json::Value;

/// POST JSON body helper result.
#[derive(Debug, Clone)]
pub struct HttpPostResult {
    /// HTTP status.
    pub status: u16,
    /// Response body text (truncated).
    pub body: String,
}

/// Build a safe outbound JSON envelope (no secrets).
pub fn outbound_envelope(event: &routerai::Event) -> Value {
    serde_json::json!({
        "id": event.id.to_string(),
        "event_type": event.event_type,
        "source": event.source,
        "timestamp": event.timestamp,
        "payload": event.payload,
        "correlation_id": event.correlation_id,
        "causation_id": event.causation_id,
        "metadata": event.metadata,
    })
}
