//! Agent memory — facts the agent may use (separate from State / Run / Event).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::RouterResult;
use crate::ids::AgentId;

/// One memory item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    /// Id.
    pub id: String,
    /// Agent.
    pub agent_id: AgentId,
    /// Optional subject scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    /// Content.
    pub content: String,
    /// Optional tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Created.
    pub created_at: DateTime<Utc>,
}

/// In-memory memory store.
#[derive(Clone, Default)]
pub struct AgentMemoryStore {
    items: Arc<RwLock<HashMap<String, MemoryItem>>>,
}

impl AgentMemoryStore {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add.
    pub async fn add(
        &self,
        agent_id: AgentId,
        content: impl Into<String>,
        subject_id: Option<String>,
        tags: Vec<String>,
    ) -> RouterResult<MemoryItem> {
        let item = MemoryItem {
            id: format!("mem_{}", Uuid::new_v4().simple()),
            agent_id,
            subject_id,
            content: content.into(),
            tags,
            created_at: Utc::now(),
        };
        self.items
            .write()
            .await
            .insert(item.id.clone(), item.clone());
        Ok(item)
    }

    /// List for agent (optionally filtered by subject).
    pub async fn list(
        &self,
        agent_id: &AgentId,
        subject_id: Option<&str>,
    ) -> Vec<MemoryItem> {
        self.items
            .read()
            .await
            .values()
            .filter(|m| &m.agent_id == agent_id)
            .filter(|m| match subject_id {
                Some(s) => m.subject_id.as_deref() == Some(s),
                None => true,
            })
            .cloned()
            .collect()
    }

    /// Delete.
    pub async fn remove(&self, id: &str) -> bool {
        self.items.write().await.remove(id).is_some()
    }
}
