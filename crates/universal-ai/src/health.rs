//! Provider health status and monitoring.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::error::AiResult;
use crate::events::{AiEvent, EventBus};
use crate::provider::DynProvider;
use crate::types::ProviderId;

/// Result of a health probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Healthy flag.
    pub healthy: bool,
    /// Probe latency.
    pub latency_ms: Option<u64>,
    /// When checked.
    pub checked_at: DateTime<Utc>,
    /// Safe error string.
    pub error: Option<String>,
}

impl HealthStatus {
    /// Healthy instant.
    pub fn ok(latency_ms: u64) -> Self {
        Self {
            healthy: true,
            latency_ms: Some(latency_ms),
            checked_at: Utc::now(),
            error: None,
        }
    }

    /// Unhealthy.
    pub fn down(error: impl Into<String>) -> Self {
        Self {
            healthy: false,
            latency_ms: None,
            checked_at: Utc::now(),
            error: Some(error.into()),
        }
    }
}

/// Tracks provider health and can exclude unhealthy ones from routing.
pub struct HealthMonitor {
    status: RwLock<HashMap<String, HealthStatus>>,
    events: Option<Arc<EventBus>>,
}

impl HealthMonitor {
    /// Create.
    pub fn new(events: Option<Arc<EventBus>>) -> Self {
        Self {
            status: RwLock::new(HashMap::new()),
            events,
        }
    }

    /// Latest status.
    pub async fn get(&self, id: &ProviderId) -> Option<HealthStatus> {
        self.status.read().await.get(id.as_str()).cloned()
    }

    /// All statuses.
    pub async fn list(&self) -> HashMap<String, HealthStatus> {
        self.status.read().await.clone()
    }

    /// Whether provider is considered usable (unknown => true).
    pub async fn is_healthy(&self, id: &ProviderId) -> bool {
        self.status
            .read()
            .await
            .get(id.as_str())
            .map(|s| s.healthy)
            .unwrap_or(true)
    }

    /// Probe one provider.
    pub async fn check(&self, provider: &DynProvider) -> AiResult<HealthStatus> {
        let started = Instant::now();
        let status = match provider.health().await {
            Ok(mut s) => {
                if s.latency_ms.is_none() {
                    s.latency_ms = Some(started.elapsed().as_millis() as u64);
                }
                s
            }
            Err(err) => HealthStatus::down(err.to_string()),
        };
        self.status
            .write()
            .await
            .insert(provider.id().to_string(), status.clone());
        if let Some(bus) = &self.events {
            if status.healthy {
                bus.emit(AiEvent::ProviderHealthy(provider.id()));
            } else {
                bus.emit(AiEvent::ProviderUnhealthy(provider.id()));
            }
        }
        Ok(status)
    }

    /// Probe all.
    pub async fn check_all(&self, providers: &[DynProvider]) -> Vec<(ProviderId, HealthStatus)> {
        let mut out = Vec::new();
        for p in providers {
            if let Ok(s) = self.check(p).await {
                out.push((p.id(), s));
            }
        }
        out
    }

    /// Background loop handle.
    pub fn spawn_loop(
        self: Arc<Self>,
        providers: Vec<DynProvider>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.check_all(&providers).await;
                tokio::time::sleep(interval).await;
            }
        })
    }
}
