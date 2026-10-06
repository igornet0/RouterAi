//! Append-only audit log for control-plane actions (incl. publish overrides).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::store::Store;

/// Audit entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Id.
    pub id: String,
    /// Action name.
    pub action: String,
    /// Actor (user/system).
    pub actor: String,
    /// Optional agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Detail JSON.
    pub detail: serde_json::Value,
    /// Timestamp.
    pub at: DateTime<Utc>,
}

/// In-memory audit log with optional durable write-through.
#[derive(Clone)]
pub struct AuditLog {
    entries: Arc<RwLock<VecDeque<AuditEntry>>>,
    capacity: usize,
    store: Option<Arc<dyn Store>>,
}

impl AuditLog {
    /// Create with capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::new())),
            capacity: capacity.max(64),
            store: None,
        }
    }

    /// Persist each record to the store.
    pub fn with_store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Seed history from persistence (oldest → newest; keep newest within capacity).
    pub async fn seed(&self, entries: impl IntoIterator<Item = AuditEntry>) {
        let mut g = self.entries.write().await;
        g.clear();
        for entry in entries {
            if g.len() >= self.capacity {
                g.pop_front();
            }
            g.push_back(entry);
        }
    }

    /// Record.
    pub async fn record(
        &self,
        action: impl Into<String>,
        actor: impl Into<String>,
        agent_id: Option<String>,
        detail: serde_json::Value,
    ) -> AuditEntry {
        let entry = AuditEntry {
            id: format!("aud_{}", Uuid::new_v4().simple()),
            action: action.into(),
            actor: actor.into(),
            agent_id,
            detail,
            at: Utc::now(),
        };
        {
            let mut g = self.entries.write().await;
            if g.len() >= self.capacity {
                g.pop_front();
            }
            g.push_back(entry.clone());
        }
        if let Some(store) = &self.store {
            if let Err(err) = store.save_audit(&entry).await {
                tracing::warn!(error = %err, "failed to persist audit entry");
            }
        }
        tracing::info!(
            audit_id = %entry.id,
            action = %entry.action,
            actor = %entry.actor,
            agent_id = ?entry.agent_id,
            "audit"
        );
        entry
    }

    /// Recent entries (newest first).
    pub async fn list(&self, limit: usize) -> Vec<AuditEntry> {
        self.entries
            .read()
            .await
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new(2000)
    }
}
