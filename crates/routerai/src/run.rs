//! Agent runs + trace steps.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AgentId, EventId, RunId};
use crate::usage_bridge::{RunCost, RunUsage};

/// Run lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Queued.
    Pending,
    /// Executing.
    Running,
    /// Success.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Timed out.
    TimedOut,
}

/// Kind of trace step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStepKind {
    /// Event received / start.
    Event,
    /// LLM call.
    Llm,
    /// Tool invocation.
    Tool,
    /// Observation / decision note.
    Observe,
    /// Final.
    Result,
    /// Error.
    Error,
}

/// One observability step inside a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStep {
    /// Ordinal.
    pub index: u32,
    /// Kind.
    pub kind: RunStepKind,
    /// Human summary (no secrets).
    pub summary: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    /// Timestamp.
    pub at: DateTime<Utc>,
    /// Optional step cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Decimal>,
}

impl RunStep {
    /// Helper.
    pub fn new(index: u32, kind: RunStepKind, summary: impl Into<String>) -> Self {
        Self {
            index,
            kind,
            summary: summary.into(),
            detail: None,
            at: Utc::now(),
            cost: None,
        }
    }
}

/// A single agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    /// Id.
    pub id: RunId,
    /// Agent.
    pub agent_id: AgentId,
    /// Status.
    pub status: RunStatus,
    /// Triggering event id if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<EventId>,
    /// Started.
    pub started_at: DateTime<Utc>,
    /// Finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Input context.
    pub input: Value,
    /// Output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Error payload (safe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// Aggregated usage.
    #[serde(default)]
    pub usage: RunUsage,
    /// Total cost USD.
    #[serde(default)]
    pub cost: Decimal,
    /// Trace.
    #[serde(default)]
    pub steps: Vec<RunStep>,
}

impl AgentRun {
    /// New pending run.
    pub fn new(agent_id: AgentId, input: Value, event_id: Option<EventId>) -> Self {
        Self {
            id: RunId::new(),
            agent_id,
            status: RunStatus::Pending,
            event_id,
            started_at: Utc::now(),
            finished_at: None,
            input,
            output: None,
            error: None,
            usage: RunUsage::default(),
            cost: Decimal::ZERO,
            steps: Vec::new(),
        }
    }

    /// Push step.
    pub fn push_step(&mut self, kind: RunStepKind, summary: impl Into<String>) {
        let idx = self.steps.len() as u32;
        self.steps.push(RunStep::new(idx, kind, summary));
    }

    /// Mark completed.
    pub fn complete(&mut self, output: Value) {
        self.status = RunStatus::Completed;
        self.output = Some(output);
        self.finished_at = Some(Utc::now());
    }

    /// Mark failed.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.status = RunStatus::Failed;
        self.error = Some(serde_json::json!({ "message": message.into() }));
        self.finished_at = Some(Utc::now());
    }

    /// Cancel.
    pub fn cancel(&mut self) {
        self.status = RunStatus::Cancelled;
        self.finished_at = Some(Utc::now());
    }

    /// Add model usage/cost.
    pub fn add_model_usage(&mut self, usage: RunUsage, cost: RunCost) {
        self.usage.prompt_tokens += usage.prompt_tokens;
        self.usage.completion_tokens += usage.completion_tokens;
        self.usage.total_tokens += usage.total_tokens;
        self.cost += cost.amount;
    }
}
