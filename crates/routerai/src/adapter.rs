//! Adapter contracts — EventSource / EventSink stay channel-agnostic in core.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::RouterResult;
use crate::event::Event;

/// Inbound channel that produces platform [`Event`]s (Telegram, webhook, …).
///
/// Adapters live outside core; this trait documents the contract only.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Adapter id (e.g. `webhook`, `telegram`).
    fn id(&self) -> &str;
}

/// Outbound delivery of platform events to an external system.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Sink id.
    fn id(&self) -> &str;

    /// Deliver one event. Errors are logged by the runtime fan-out; they do not fail emit.
    async fn deliver(&self, event: &Event) -> RouterResult<()>;
}

/// Registered outbound webhook / HTTP callback target (channel-agnostic config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkTarget {
    /// Id.
    pub id: String,
    /// Destination URL (HTTP POST).
    pub url: String,
    /// Event types to match (exact). Empty = match all.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Optional shared secret sent as `X-RouterAi-Sink-Secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Enabled?
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl SinkTarget {
    /// New target.
    pub fn new(url: impl Into<String>, event_types: Vec<String>) -> Self {
        Self {
            id: format!("sink_{}", Uuid::new_v4().simple()),
            url: url.into(),
            event_types,
            secret: None,
            enabled: true,
        }
    }

    /// Does this target want the event?
    pub fn matches(&self, event_type: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if self.event_types.is_empty() {
            return true;
        }
        self.event_types.iter().any(|t| {
            if let Some(prefix) = t.strip_suffix(".*") {
                event_type.starts_with(prefix)
            } else {
                t == event_type
            }
        })
    }
}

/// In-memory sink target registry + pluggable [`EventSink`] implementations.
#[derive(Clone, Default)]
pub struct SinkRegistry {
    targets: Arc<RwLock<HashMap<String, SinkTarget>>>,
    sinks: Arc<RwLock<Vec<Arc<dyn EventSink>>>>,
}

impl SinkRegistry {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a sink implementation (e.g. HTTP webhook deliverer).
    pub async fn add_sink(&self, sink: Arc<dyn EventSink>) {
        self.sinks.write().await.push(sink);
    }

    /// Upsert a destination target.
    pub async fn upsert_target(&self, target: SinkTarget) -> SinkTarget {
        self.targets
            .write()
            .await
            .insert(target.id.clone(), target.clone());
        target
    }

    /// Remove target.
    pub async fn remove_target(&self, id: &str) -> bool {
        self.targets.write().await.remove(id).is_some()
    }

    /// List targets.
    pub async fn list_targets(&self) -> Vec<SinkTarget> {
        self.targets.read().await.values().cloned().collect()
    }

    /// Get one.
    pub async fn get_target(&self, id: &str) -> Option<SinkTarget> {
        self.targets.read().await.get(id).cloned()
    }

    /// Matching enabled targets for an event type.
    pub async fn matching_targets(&self, event_type: &str) -> Vec<SinkTarget> {
        self.targets
            .read()
            .await
            .values()
            .filter(|t| t.matches(event_type))
            .cloned()
            .collect()
    }

    /// Fan-out to all registered sinks (best-effort).
    pub async fn deliver_all(&self, event: &Event) {
        let sinks = self.sinks.read().await.clone();
        for sink in sinks {
            if let Err(err) = sink.deliver(event).await {
                tracing::warn!(
                    sink = sink.id(),
                    event_type = %event.event_type,
                    error = %err,
                    "sink delivery failed"
                );
            }
        }
    }
}
