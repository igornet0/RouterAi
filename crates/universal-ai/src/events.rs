//! Global event bus.

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::balance::Balance;
use crate::models::ModelInfo;
use crate::rate_limit::RateLimit;
use crate::types::{ModelId, ProviderId, RequestId};
use crate::usage::Usage;

/// Library-wide events.
#[derive(Debug, Clone)]
pub enum AiEvent {
    /// Request started.
    RequestStarted(RequestStarted),
    /// Request completed.
    RequestCompleted(RequestCompleted),
    /// Request failed.
    RequestFailed(RequestFailed),
    /// Balance updated.
    BalanceUpdated(Balance),
    /// Balance warning.
    BalanceWarning(Balance),
    /// Balance critical.
    BalanceCritical(Balance),
    /// Model discovered.
    ModelDiscovered(ModelInfo),
    /// Provider healthy.
    ProviderHealthy(ProviderId),
    /// Provider unhealthy.
    ProviderUnhealthy(ProviderId),
    /// Rate limit snapshot.
    RateLimitUpdated {
        /// Provider.
        provider: ProviderId,
        /// Limits.
        rate_limit: RateLimit,
    },
}

/// Request started payload.
#[derive(Debug, Clone)]
pub struct RequestStarted {
    /// Id.
    pub request_id: RequestId,
    /// Provider.
    pub provider: ProviderId,
    /// Model.
    pub model: ModelId,
    /// Time.
    pub at: DateTime<Utc>,
}

/// Request completed payload.
#[derive(Debug, Clone)]
pub struct RequestCompleted {
    /// Id.
    pub request_id: RequestId,
    /// Provider.
    pub provider: ProviderId,
    /// Model.
    pub model: ModelId,
    /// Usage.
    pub usage: Option<Usage>,
    /// Latency.
    pub latency_ms: u64,
    /// Time.
    pub at: DateTime<Utc>,
}

/// Request failed payload (no secrets).
#[derive(Debug, Clone)]
pub struct RequestFailed {
    /// Id.
    pub request_id: RequestId,
    /// Provider.
    pub provider: Option<ProviderId>,
    /// Safe message.
    pub message: String,
    /// Time.
    pub at: DateTime<Utc>,
}

/// Broadcast event bus.
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<AiEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventBus {
    /// Create with capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit.
    pub fn emit(&self, event: AiEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<AiEvent> {
        self.tx.subscribe()
    }
}
