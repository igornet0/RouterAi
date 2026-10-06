//! Agent entity (not the model runtime).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{RouterError, RouterResult};
use crate::ids::AgentId;
use crate::tool::Permissions;

/// Production lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Editable draft.
    #[default]
    Draft,
    /// Under Test Lab.
    Testing,
    /// Tests done, ready to publish.
    Ready,
    /// Live for automated handlers.
    Published,
    /// Temporarily stopped.
    Paused,
    /// Soft-deleted / retired.
    Archived,
    /// Legacy alias for paused.
    Disabled,
}

/// How a run was triggered — affects lifecycle gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Handler / schedule / webhook automation — requires Published.
    #[default]
    Automated,
    /// Playground / Test Lab / manual — allows Draft/Testing/Ready/Published.
    Interactive,
}

/// Which model to use via universal-ai.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelPolicy {
    /// Preferred provider id (e.g. `deepseek`).
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Optional temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional max tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Default for AgentModelPolicy {
    fn default() -> Self {
        Self {
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            temperature: Some(0.2),
            max_tokens: Some(1024),
        }
    }
}

/// Run limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLimits {
    /// Max agent loop steps.
    pub max_steps: u32,
    /// Wall-clock max seconds.
    pub max_runtime_seconds: u64,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_runtime_seconds: 300,
        }
    }
}

/// Per-agent budget.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentBudget {
    /// Max cost for a single run (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_cost: Option<Decimal>,
    /// Max daily spend for this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_daily_cost: Option<Decimal>,
}

/// Agent definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Id of this version record.
    pub id: AgentId,
    /// Stable lineage id across versions (equals first version id).
    #[serde(default)]
    pub lineage_id: AgentId,
    /// Name.
    pub name: String,
    /// System instructions.
    pub instructions: String,
    /// Model policy (resolved through universal-ai).
    pub model: AgentModelPolicy,
    /// Allowed tool ids.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tool / capability permissions.
    #[serde(default)]
    pub permissions: Permissions,
    /// Limits.
    #[serde(default)]
    pub limits: AgentLimits,
    /// Budget.
    #[serde(default)]
    pub budget: AgentBudget,
    /// Status.
    #[serde(default)]
    pub status: AgentStatus,
    /// Version counter within lineage.
    #[serde(default = "one")]
    pub version: u32,
    /// Last known regression pass (factual flag for publish checks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_regression_passed: Option<bool>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Updated.
    pub updated_at: DateTime<Utc>,
}

fn one() -> u32 {
    1
}

impl Agent {
    /// Create a draft agent (lineage_id = id).
    pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        let now = Utc::now();
        let id = AgentId::new();
        Self {
            id: id.clone(),
            lineage_id: id,
            name: name.into(),
            instructions: instructions.into(),
            model: AgentModelPolicy::default(),
            tools: Vec::new(),
            permissions: Permissions::ai_safe(),
            limits: AgentLimits::default(),
            budget: AgentBudget::default(),
            status: AgentStatus::Draft,
            version: 1,
            last_regression_passed: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Mark published (same version).
    pub fn publish(mut self) -> Self {
        self.status = AgentStatus::Published;
        self.updated_at = Utc::now();
        self
    }

    /// Create a new draft revision (new id, version+1, same lineage).
    pub fn revise(&self) -> Self {
        let now = Utc::now();
        Self {
            id: AgentId::new(),
            lineage_id: self.lineage_id.clone(),
            name: self.name.clone(),
            instructions: self.instructions.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            permissions: self.permissions.clone(),
            limits: self.limits.clone(),
            budget: self.budget.clone(),
            status: AgentStatus::Draft,
            version: self.version.saturating_add(1),
            last_regression_passed: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Automated handler runs — Published only.
    pub fn ensure_runnable(&self) -> RouterResult<()> {
        self.ensure_runnable_mode(RunMode::Automated)
    }

    /// Gate by run mode.
    pub fn ensure_runnable_mode(&self, mode: RunMode) -> RouterResult<()> {
        match mode {
            RunMode::Automated => match self.status {
                AgentStatus::Published => Ok(()),
                AgentStatus::Paused => Err(RouterError::Forbidden("agent is paused".into())),
                AgentStatus::Archived => Err(RouterError::Forbidden("agent is archived".into())),
                AgentStatus::Disabled => Err(RouterError::Forbidden("agent disabled".into())),
                AgentStatus::Draft => Err(RouterError::Forbidden(
                    "agent is draft — publish first".into(),
                )),
                AgentStatus::Testing | AgentStatus::Ready => Err(RouterError::Forbidden(
                    "agent not published — automated runs require Published".into(),
                )),
            },
            RunMode::Interactive => match self.status {
                AgentStatus::Draft
                | AgentStatus::Testing
                | AgentStatus::Ready
                | AgentStatus::Published => Ok(()),
                AgentStatus::Paused => Err(RouterError::Forbidden("agent is paused".into())),
                AgentStatus::Archived => Err(RouterError::Forbidden("agent is archived".into())),
                AgentStatus::Disabled => Err(RouterError::Forbidden("agent disabled".into())),
            },
        }
    }
}

/// In-memory agent registry (all versions).
#[derive(Clone, Default)]
pub struct AgentStore {
    agents: Arc<RwLock<HashMap<String, Agent>>>,
}

impl AgentStore {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert a version record.
    pub async fn upsert(&self, mut agent: Agent) -> RouterResult<Agent> {
        if agent.lineage_id.as_str().is_empty() {
            agent.lineage_id = agent.id.clone();
        }
        self.agents
            .write()
            .await
            .insert(agent.id.to_string(), agent.clone());
        Ok(agent)
    }

    /// Get by version id.
    pub async fn get(&self, id: &AgentId) -> Option<Agent> {
        self.agents.read().await.get(id.as_str()).cloned()
    }

    /// All version records.
    pub async fn list(&self) -> Vec<Agent> {
        self.agents.read().await.values().cloned().collect()
    }

    /// Latest version per lineage (highest version).
    pub async fn list_latest(&self) -> Vec<Agent> {
        let mut best: HashMap<String, Agent> = HashMap::new();
        for a in self.list().await {
            let key = a.lineage_id.to_string();
            match best.get(&key) {
                Some(prev) if prev.version >= a.version => {}
                _ => {
                    best.insert(key, a);
                }
            }
        }
        best.into_values().collect()
    }

    /// All versions for a lineage.
    pub async fn versions(&self, lineage_id: &AgentId) -> Vec<Agent> {
        let mut v: Vec<_> = self
            .list()
            .await
            .into_iter()
            .filter(|a| &a.lineage_id == lineage_id)
            .collect();
        v.sort_by(|a, b| a.version.cmp(&b.version));
        v
    }

    /// Delete one version.
    pub async fn remove(&self, id: &AgentId) -> RouterResult<()> {
        if self.agents.write().await.remove(id.as_str()).is_none() {
            return Err(RouterError::NotFound(format!("agent {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revise_bumps_version_keeps_lineage() {
        let a = Agent::new("sales", "hi").publish();
        let b = a.revise();
        assert_eq!(b.lineage_id, a.lineage_id);
        assert_ne!(b.id, a.id);
        assert_eq!(b.version, 2);
        assert_eq!(b.status, AgentStatus::Draft);
    }

    #[test]
    fn interactive_allows_draft() {
        let a = Agent::new("x", "y");
        assert!(a.ensure_runnable_mode(RunMode::Interactive).is_ok());
        assert!(a.ensure_runnable_mode(RunMode::Automated).is_err());
    }
}
