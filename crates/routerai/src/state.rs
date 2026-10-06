//! Long-lived agent state (separate from Event / Run / Memory).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::RouterResult;
use crate::ids::AgentId;

/// Current state of a long-lived subject under an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Agent lineage or version id.
    pub agent_id: AgentId,
    /// Subject (e.g. customer id, chat id).
    pub subject_id: String,
    /// Free-form state document.
    pub data: Value,
    /// Updated.
    pub updated_at: DateTime<Utc>,
}

impl AgentState {
    /// New empty state.
    pub fn new(agent_id: AgentId, subject_id: impl Into<String>) -> Self {
        Self {
            agent_id,
            subject_id: subject_id.into(),
            data: Value::Object(Default::default()),
            updated_at: Utc::now(),
        }
    }
}

/// In-memory state store.
#[derive(Clone, Default)]
pub struct StateStore {
    map: Arc<RwLock<HashMap<String, AgentState>>>,
}

impl StateStore {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    fn key(agent_id: &AgentId, subject_id: &str) -> String {
        format!("{agent_id}:{subject_id}")
    }

    /// Get.
    pub async fn get(&self, agent_id: &AgentId, subject_id: &str) -> Option<AgentState> {
        self.map
            .read()
            .await
            .get(&Self::key(agent_id, subject_id))
            .cloned()
    }

    /// Upsert.
    pub async fn upsert(&self, mut state: AgentState) -> RouterResult<AgentState> {
        state.updated_at = Utc::now();
        let k = Self::key(&state.agent_id, &state.subject_id);
        self.map.write().await.insert(k, state.clone());
        Ok(state)
    }

    /// List for agent.
    pub async fn list_for_agent(&self, agent_id: &AgentId) -> Vec<AgentState> {
        self.map
            .read()
            .await
            .values()
            .filter(|s| &s.agent_id == agent_id)
            .cloned()
            .collect()
    }
}
