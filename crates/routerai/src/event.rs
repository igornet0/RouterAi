//! Event envelope + in-process bus.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::error::RouterResult;
use crate::ids::EventId;

/// Arbitrary metadata bag.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventMetadata {
    /// Logical account / tenant hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Extra string map.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extra: std::collections::HashMap<String, String>,
}

/// Strict event envelope; payload is arbitrary JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Id.
    pub id: EventId,
    /// Type, e.g. `telegram.message.received`.
    pub event_type: String,
    /// Source system.
    pub source: String,
    /// When produced.
    pub timestamp: DateTime<Utc>,
    /// Arbitrary payload.
    pub payload: Value,
    /// Metadata.
    #[serde(default)]
    pub metadata: EventMetadata,
    /// Correlation across a chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Parent event id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
}

impl Event {
    /// Build a new event.
    pub fn new(
        event_type: impl Into<String>,
        source: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            id: EventId::new(),
            event_type: event_type.into(),
            source: source.into(),
            timestamp: Utc::now(),
            payload,
            metadata: EventMetadata::default(),
            correlation_id: None,
            causation_id: None,
        }
    }

    /// Chain from a parent.
    pub fn cause_of(mut self, parent: &Event) -> Self {
        self.causation_id = Some(parent.id.to_string());
        self.correlation_id = parent
            .correlation_id
            .clone()
            .or_else(|| Some(parent.id.to_string()));
        self
    }

    /// Set account metadata.
    pub fn with_account(mut self, account: impl Into<String>) -> Self {
        self.metadata.account = Some(account.into());
        self
    }
}

/// Event bus: emit + subscribe + recent history.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    history: Arc<RwLock<VecDeque<Event>>>,
    capacity: usize,
}

impl EventBus {
    /// Create with broadcast + history capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            tx,
            history: Arc::new(RwLock::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Emit and retain in history.
    pub async fn emit(&self, event: Event) -> RouterResult<Event> {
        {
            let mut h = self.history.write().await;
            if h.len() >= self.capacity {
                h.pop_front();
            }
            h.push_back(event.clone());
        }
        let _ = self.tx.send(event.clone());
        tracing::info!(
            event_id = %event.id,
            event_type = %event.event_type,
            source = %event.source,
            "event emitted"
        );
        Ok(event)
    }

    /// Load history from persistence without broadcasting (startup hydrate).
    ///
    /// Events should be in chronological order (oldest first). Excess beyond
    /// capacity keeps the newest.
    pub async fn seed_history(&self, events: impl IntoIterator<Item = Event>) {
        let mut h = self.history.write().await;
        h.clear();
        for event in events {
            if h.len() >= self.capacity {
                h.pop_front();
            }
            h.push_back(event);
        }
    }

    /// Subscribe to live events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Recent history (newest last).
    pub async fn list(&self, limit: usize) -> Vec<Event> {
        let h = self.history.read().await;
        h.iter().rev().take(limit).cloned().collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// Get by id.
    pub async fn get(&self, id: &EventId) -> Option<Event> {
        self.history
            .read()
            .await
            .iter()
            .find(|e| &e.id == id)
            .cloned()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn emit_and_list() {
        let bus = EventBus::new(10);
        let e = Event::new("telegram.message.received", "telegram", json!({"text": "hi"}));
        bus.emit(e.clone()).await.unwrap();
        let list = bus.list(10).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].event_type, "telegram.message.received");
    }
}
