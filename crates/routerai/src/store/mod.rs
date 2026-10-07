//! Persistence abstraction.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::adapter::SinkTarget;
use crate::agent::Agent;
use crate::audit::AuditEntry;
use crate::error::{RouterError, RouterResult};
use crate::eval::{Dataset, TestCase};
use crate::event::Event;
use crate::handler::Handler;
use crate::ids::{AgentId, EventId, HandlerId, RunId, ScheduleId};
use crate::run::AgentRun;
use crate::scheduler::Schedule;

/// Storage for platform entities.
#[async_trait]
pub trait Store: Send + Sync {
    /// Save event.
    async fn save_event(&self, event: &Event) -> RouterResult<()>;
    /// List events.
    async fn list_events(&self, limit: usize) -> RouterResult<Vec<Event>>;
    /// Get event.
    async fn get_event(&self, id: &EventId) -> RouterResult<Option<Event>>;

    /// Save handler.
    async fn save_handler(&self, handler: &Handler) -> RouterResult<()>;
    /// List handlers.
    async fn list_handlers(&self) -> RouterResult<Vec<Handler>>;
    /// Get handler.
    async fn get_handler(&self, id: &HandlerId) -> RouterResult<Option<Handler>>;
    /// Delete handler.
    async fn delete_handler(&self, id: &HandlerId) -> RouterResult<()>;

    /// Save agent.
    async fn save_agent(&self, agent: &Agent) -> RouterResult<()>;
    /// List agents.
    async fn list_agents(&self) -> RouterResult<Vec<Agent>>;
    /// Get agent.
    async fn get_agent(&self, id: &AgentId) -> RouterResult<Option<Agent>>;
    /// Delete agent.
    async fn delete_agent(&self, id: &AgentId) -> RouterResult<()>;

    /// Save run.
    async fn save_run(&self, run: &AgentRun) -> RouterResult<()>;
    /// List runs.
    async fn list_runs(&self, limit: usize) -> RouterResult<Vec<AgentRun>>;
    /// Get run.
    async fn get_run(&self, id: &RunId) -> RouterResult<Option<AgentRun>>;

    /// Save schedule.
    async fn save_schedule(&self, schedule: &Schedule) -> RouterResult<()>;
    /// List schedules.
    async fn list_schedules(&self) -> RouterResult<Vec<Schedule>>;
    /// Delete schedule.
    async fn delete_schedule(&self, id: &ScheduleId) -> RouterResult<()>;

    /// Save webhook / sink target.
    async fn save_sink_target(&self, target: &SinkTarget) -> RouterResult<()>;
    /// List sink targets.
    async fn list_sink_targets(&self) -> RouterResult<Vec<SinkTarget>>;
    /// Delete sink target.
    async fn delete_sink_target(&self, id: &str) -> RouterResult<()>;

    /// Save test case.
    async fn save_test_case(&self, case: &TestCase) -> RouterResult<()>;
    /// List test cases.
    async fn list_test_cases(&self) -> RouterResult<Vec<TestCase>>;
    /// Delete test case.
    async fn delete_test_case(&self, id: &str) -> RouterResult<()>;

    /// Save dataset.
    async fn save_dataset(&self, dataset: &Dataset) -> RouterResult<()>;
    /// List datasets.
    async fn list_datasets(&self) -> RouterResult<Vec<Dataset>>;
    /// Delete dataset.
    async fn delete_dataset(&self, id: &str) -> RouterResult<()>;

    /// Append audit entry.
    async fn save_audit(&self, entry: &AuditEntry) -> RouterResult<()>;
    /// List audit entries (newest first).
    async fn list_audit(&self, limit: usize) -> RouterResult<Vec<AuditEntry>>;

    /// Read a settings key.
    async fn get_setting(&self, key: &str) -> RouterResult<Option<String>>;
    /// Write a settings key.
    async fn set_setting(&self, key: &str, value: &str) -> RouterResult<()>;
}

/// Kill-switch settings key.
pub const SETTING_KILL_SWITCH: &str = "kill_switch";

/// In-memory store.
#[derive(Clone, Default)]
pub struct MemoryStore {
    events: Arc<RwLock<Vec<Event>>>,
    handlers: Arc<RwLock<HashMap<String, Handler>>>,
    agents: Arc<RwLock<HashMap<String, Agent>>>,
    runs: Arc<RwLock<HashMap<String, AgentRun>>>,
    schedules: Arc<RwLock<HashMap<String, Schedule>>>,
    sink_targets: Arc<RwLock<HashMap<String, SinkTarget>>>,
    test_cases: Arc<RwLock<HashMap<String, TestCase>>>,
    datasets: Arc<RwLock<HashMap<String, Dataset>>>,
    audit: Arc<RwLock<Vec<AuditEntry>>>,
    settings: Arc<RwLock<HashMap<String, String>>>,
}

impl MemoryStore {
    /// Create.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn save_event(&self, event: &Event) -> RouterResult<()> {
        self.events.write().await.push(event.clone());
        Ok(())
    }
    async fn list_events(&self, limit: usize) -> RouterResult<Vec<Event>> {
        let g = self.events.read().await;
        Ok(g.iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect())
    }
    async fn get_event(&self, id: &EventId) -> RouterResult<Option<Event>> {
        Ok(self
            .events
            .read()
            .await
            .iter()
            .find(|e| &e.id == id)
            .cloned())
    }
    async fn save_handler(&self, handler: &Handler) -> RouterResult<()> {
        self.handlers
            .write()
            .await
            .insert(handler.id.to_string(), handler.clone());
        Ok(())
    }
    async fn list_handlers(&self) -> RouterResult<Vec<Handler>> {
        Ok(self.handlers.read().await.values().cloned().collect())
    }
    async fn get_handler(&self, id: &HandlerId) -> RouterResult<Option<Handler>> {
        Ok(self.handlers.read().await.get(id.as_str()).cloned())
    }
    async fn delete_handler(&self, id: &HandlerId) -> RouterResult<()> {
        self.handlers.write().await.remove(id.as_str());
        Ok(())
    }
    async fn save_agent(&self, agent: &Agent) -> RouterResult<()> {
        self.agents
            .write()
            .await
            .insert(agent.id.to_string(), agent.clone());
        Ok(())
    }
    async fn list_agents(&self) -> RouterResult<Vec<Agent>> {
        Ok(self.agents.read().await.values().cloned().collect())
    }
    async fn get_agent(&self, id: &AgentId) -> RouterResult<Option<Agent>> {
        Ok(self.agents.read().await.get(id.as_str()).cloned())
    }
    async fn delete_agent(&self, id: &AgentId) -> RouterResult<()> {
        self.agents.write().await.remove(id.as_str());
        Ok(())
    }
    async fn save_run(&self, run: &AgentRun) -> RouterResult<()> {
        self.runs
            .write()
            .await
            .insert(run.id.to_string(), run.clone());
        Ok(())
    }
    async fn list_runs(&self, limit: usize) -> RouterResult<Vec<AgentRun>> {
        let mut v: Vec<_> = self.runs.read().await.values().cloned().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        v.truncate(limit);
        Ok(v)
    }
    async fn get_run(&self, id: &RunId) -> RouterResult<Option<AgentRun>> {
        Ok(self.runs.read().await.get(id.as_str()).cloned())
    }
    async fn save_schedule(&self, schedule: &Schedule) -> RouterResult<()> {
        self.schedules
            .write()
            .await
            .insert(schedule.id.to_string(), schedule.clone());
        Ok(())
    }
    async fn list_schedules(&self) -> RouterResult<Vec<Schedule>> {
        Ok(self.schedules.read().await.values().cloned().collect())
    }
    async fn delete_schedule(&self, id: &ScheduleId) -> RouterResult<()> {
        self.schedules.write().await.remove(id.as_str());
        Ok(())
    }
    async fn save_sink_target(&self, target: &SinkTarget) -> RouterResult<()> {
        self.sink_targets
            .write()
            .await
            .insert(target.id.clone(), target.clone());
        Ok(())
    }
    async fn list_sink_targets(&self) -> RouterResult<Vec<SinkTarget>> {
        Ok(self.sink_targets.read().await.values().cloned().collect())
    }
    async fn delete_sink_target(&self, id: &str) -> RouterResult<()> {
        self.sink_targets.write().await.remove(id);
        Ok(())
    }
    async fn save_test_case(&self, case: &TestCase) -> RouterResult<()> {
        self.test_cases
            .write()
            .await
            .insert(case.id.clone(), case.clone());
        Ok(())
    }
    async fn list_test_cases(&self) -> RouterResult<Vec<TestCase>> {
        Ok(self.test_cases.read().await.values().cloned().collect())
    }
    async fn delete_test_case(&self, id: &str) -> RouterResult<()> {
        self.test_cases.write().await.remove(id);
        Ok(())
    }
    async fn save_dataset(&self, dataset: &Dataset) -> RouterResult<()> {
        self.datasets
            .write()
            .await
            .insert(dataset.id.clone(), dataset.clone());
        Ok(())
    }
    async fn list_datasets(&self) -> RouterResult<Vec<Dataset>> {
        Ok(self.datasets.read().await.values().cloned().collect())
    }
    async fn delete_dataset(&self, id: &str) -> RouterResult<()> {
        self.datasets.write().await.remove(id);
        Ok(())
    }
    async fn save_audit(&self, entry: &AuditEntry) -> RouterResult<()> {
        self.audit.write().await.push(entry.clone());
        Ok(())
    }
    async fn list_audit(&self, limit: usize) -> RouterResult<Vec<AuditEntry>> {
        let g = self.audit.read().await;
        Ok(g.iter().rev().take(limit).cloned().collect())
    }
    async fn get_setting(&self, key: &str) -> RouterResult<Option<String>> {
        Ok(self.settings.read().await.get(key).cloned())
    }
    async fn set_setting(&self, key: &str, value: &str) -> RouterResult<()> {
        self.settings
            .write()
            .await
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

/// Map storage errors.
pub fn storage_err(e: impl std::fmt::Display) -> RouterError {
    RouterError::Storage(e.to_string())
}
