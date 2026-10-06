//! RouterAi — agent runtime platform on top of `universal-ai`.
//!
//! Events → Handlers → Agents → Tools → Results → Events.
//! Models are invoked only through `universal_ai::AiClient`.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod adapter;
pub mod agent;
pub mod audit;
pub mod config;
pub mod control;
pub mod error;
pub mod eval;
pub mod event;
pub mod handler;
pub mod ids;
pub mod lifecycle;
pub mod memory;
pub mod runtime;
pub mod scheduler;
pub mod state;
pub mod store;
pub mod tool;

pub use adapter::{EventSink, EventSource, SinkRegistry, SinkTarget};
pub use agent::{
    Agent, AgentBudget, AgentLimits, AgentModelPolicy, AgentStatus, AgentStore, RunMode,
};
pub use audit::{AuditEntry, AuditLog};
pub use config::RuntimeConfig;
pub use control::{ControlPlane, PublishOptions, PublishResult};
pub use error::{RouterError, RouterResult};
pub use eval::{
    text_case, Assertion, Dataset, EvaluationReport, RegressionReport, TestCase, TestCaseStore,
    TestLab,
};
pub use event::{Event, EventBus, EventMetadata};
pub use handler::{
    Action, Condition, ConditionOp, ErrorPolicy, Handler, HandlerEngine, HandlerTrigger,
};
pub use ids::{AgentId, EventId, HandlerId, RunId, ScheduleId, ToolId};
pub use lifecycle::{PublishValidationReport, ValidationCheck};
pub use memory::{AgentMemoryStore, MemoryItem};
pub use runtime::{
    published_agent, AgentDashRow, DashboardSnapshot, DoctorReport, RouterRuntime, RuntimeBuilder,
};
pub use scheduler::{Schedule, ScheduleKind, Scheduler};
pub use state::{AgentState, StateStore};
pub use store::{MemoryStore, Store};
pub use tool::{
    BuiltinTools, Permission, Permissions, ToolDefinition, ToolExecutor, ToolHandler, ToolRegistry,
};

#[cfg(feature = "sqlite")]
pub use store::SqliteStore;

pub use run::{AgentRun, RunStatus, RunStep, RunStepKind};
pub use usage_bridge::{RunCost, RunUsage};

/// README code blocks are compiled (and the offline ones run) as doctests.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;

mod agent_tools;
mod run;
mod usage_bridge;
