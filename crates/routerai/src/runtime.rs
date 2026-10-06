//! Agent + event runtime orchestrator.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use universal_ai::{AiClient, Message, ToolCall, ToolResult};

use crate::adapter::{SinkRegistry, SinkTarget};
use crate::agent::{Agent, AgentStatus, AgentStore, RunMode};
use crate::agent_tools::AgentToolset;
use crate::audit::AuditLog;
use crate::config::RuntimeConfig;
use crate::control::ControlPlane;
use crate::error::{RouterError, RouterResult};
use crate::eval::{Dataset, EvaluationReport, RegressionReport, TestCase, TestCaseStore, TestLab};
use crate::event::{Event, EventBus};
use crate::handler::{Action, ErrorPolicy, Handler, HandlerEngine};
use crate::ids::{AgentId, HandlerId, RunId};
use crate::lifecycle::{self, PublishContext, PublishValidationReport};
use crate::memory::AgentMemoryStore;
use crate::run::{AgentRun, RunStatus, RunStepKind};
use crate::scheduler::{Schedule, Scheduler};
use crate::state::StateStore;
use crate::store::{MemoryStore, Store, SETTING_KILL_SWITCH};
use crate::tool::{BuiltinTools, ToolExecutor, ToolRegistry};
use crate::usage_bridge::{RunCost, RunUsage};

/// Builder for [`RouterRuntime`].
pub struct RuntimeBuilder {
    config: RuntimeConfig,
    store: Option<Arc<dyn Store>>,
    ai: Option<Arc<AiClient>>,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Start.
    pub fn new() -> Self {
        Self {
            config: RuntimeConfig::default(),
            store: None,
            ai: None,
        }
    }

    /// Config.
    pub fn config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    /// Persistence.
    pub fn store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// universal-ai client (optional until Phase B runs).
    pub fn ai(mut self, ai: Arc<AiClient>) -> Self {
        self.ai = Some(ai);
        self
    }

    /// Build runtime (registers builtin tools).
    pub async fn build(self) -> RouterResult<RouterRuntime> {
        let store = self
            .store
            .unwrap_or_else(|| Arc::new(MemoryStore::new()) as Arc<dyn Store>);
        let bus = EventBus::new(self.config.event_capacity);
        let handlers = HandlerEngine::new();
        let agents = AgentStore::new();
        let scheduler = Scheduler::new();
        let registry = ToolRegistry::new();
        BuiltinTools::register_all(&registry).await?;
        let tools = ToolExecutor::new(registry);
        let cancelled = Arc::new(RwLock::new(HashSet::<String>::new()));
        let kill_switch = Arc::new(AtomicBool::new(self.config.kill_switch));

        let rt = RouterRuntime {
            config: self.config,
            store: Arc::clone(&store),
            bus,
            handlers,
            agents,
            scheduler,
            tools,
            ai: self.ai,
            cancelled,
            kill_switch,
            tests: TestCaseStore::new(),
            state: StateStore::new(),
            memory: AgentMemoryStore::new(),
            audit: AuditLog::new(2000).with_store(Arc::clone(&store)),
            sinks: SinkRegistry::new(),
        };
        rt.hydrate_from_store().await?;
        Ok(rt)
    }
}

/// Central RouterAi runtime.
#[derive(Clone)]
pub struct RouterRuntime {
    config: RuntimeConfig,
    store: Arc<dyn Store>,
    bus: EventBus,
    handlers: HandlerEngine,
    agents: AgentStore,
    scheduler: Scheduler,
    tools: ToolExecutor,
    ai: Option<Arc<AiClient>>,
    cancelled: Arc<RwLock<HashSet<String>>>,
    kill_switch: Arc<AtomicBool>,
    tests: TestCaseStore,
    state: StateStore,
    memory: AgentMemoryStore,
    audit: AuditLog,
    sinks: SinkRegistry,
}

impl RouterRuntime {
    /// Builder.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Kill switch (Arc-safe). Prefer [`Self::set_kill_switch_persisted`] from control plane.
    pub fn set_kill_switch(&self, on: bool) {
        self.kill_switch.store(on, Ordering::SeqCst);
    }

    /// Persist kill switch and update in-memory flag.
    pub async fn set_kill_switch_persisted(&self, on: bool) -> RouterResult<()> {
        self.store
            .set_setting(SETTING_KILL_SWITCH, if on { "true" } else { "false" })
            .await?;
        self.set_kill_switch(on);
        Ok(())
    }

    /// Is kill switch on?
    pub fn kill_switch(&self) -> bool {
        self.kill_switch.load(Ordering::SeqCst)
    }

    /// Event bus.
    pub fn events(&self) -> &EventBus {
        &self.bus
    }

    /// Handlers.
    pub fn handlers(&self) -> &HandlerEngine {
        &self.handlers
    }

    /// Agents.
    pub fn agents(&self) -> &AgentStore {
        &self.agents
    }

    /// Tools.
    pub fn tools(&self) -> &ToolExecutor {
        &self.tools
    }

    /// Scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Store.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// Test case store.
    pub fn tests(&self) -> &TestCaseStore {
        &self.tests
    }

    /// Agent state store.
    pub fn state(&self) -> &StateStore {
        &self.state
    }

    /// Agent memory store.
    pub fn memory(&self) -> &AgentMemoryStore {
        &self.memory
    }

    /// Audit log.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Outbound sink registry (webhook targets, etc.).
    pub fn sinks(&self) -> &SinkRegistry {
        &self.sinks
    }

    /// Control plane facade.
    pub fn control(&self) -> ControlPlane<'_> {
        ControlPlane::new(self)
    }

    /// Optional AI client.
    pub fn ai(&self) -> Option<&AiClient> {
        self.ai.as_deref()
    }

    /// Load durable state from the backing store into hot registries.
    pub async fn hydrate_from_store(&self) -> RouterResult<()> {
        if let Some(v) = self.store.get_setting(SETTING_KILL_SWITCH).await? {
            self.set_kill_switch(v == "true" || v == "1");
        }

        let agents = self.store.list_agents().await?;
        for agent in agents {
            self.agents.upsert(agent).await?;
        }

        let handlers = self.store.list_handlers().await?;
        for handler in handlers {
            self.handlers.upsert(handler).await?;
        }

        let schedules = self.store.list_schedules().await?;
        for schedule in schedules {
            self.scheduler.upsert(schedule).await?;
        }

        let targets = self.store.list_sink_targets().await?;
        for target in targets {
            self.sinks.upsert_target(target).await;
        }

        let cases = self.store.list_test_cases().await?;
        for case in cases {
            self.tests.upsert_case(case).await?;
        }

        let datasets = self.store.list_datasets().await?;
        for ds in datasets {
            self.tests.upsert_dataset(ds).await?;
        }

        // Store returns newest-first; seed oldest→newest within capacity.
        let mut audit = self.store.list_audit(2000).await?;
        audit.reverse();
        self.audit.seed(audit).await;

        if self.config.persist_events {
            let events = self.store.list_events(self.config.event_capacity).await?;
            self.bus.seed_history(events).await;
        }

        // Incomplete runs cannot resume — mark failed so the console stays truthful.
        let runs = self.store.list_runs(500).await?;
        for mut run in runs {
            if matches!(run.status, RunStatus::Pending | RunStatus::Running) {
                run.fail("interrupted by process restart");
                self.store.save_run(&run).await?;
            }
        }

        Ok(())
    }

    /// Validate agent for publish.
    pub async fn validate_publish(&self, id: &AgentId) -> RouterResult<PublishValidationReport> {
        let agent = self
            .agents
            .get(id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("agent {id}")))?;
        let cases = self.tests.list_cases(Some(id)).await;
        let doctor = self.doctor().await;
        lifecycle::validate_publish(
            &agent,
            PublishContext {
                tool_registry: self.tools.registry(),
                ai_configured: self.ai.is_some(),
                kill_switch: self.kill_switch(),
                runtime_ok: doctor.runtime_ok,
                regression_passed: agent.last_regression_passed,
                test_case_count: cases.len(),
            },
        )
        .await
    }

    /// Upsert agent + persist.
    pub async fn upsert_agent(&self, agent: Agent) -> RouterResult<Agent> {
        let agent = self.agents.upsert(agent).await?;
        self.store.save_agent(&agent).await?;
        Ok(agent)
    }

    /// Delete agent version.
    pub async fn delete_agent(&self, id: &AgentId) -> RouterResult<()> {
        self.agents.remove(id).await?;
        self.store.delete_agent(id).await?;
        Ok(())
    }

    /// Publish agent (Draft → Published).
    pub async fn publish_agent(&self, id: &AgentId) -> RouterResult<Agent> {
        let agent = self
            .agents
            .get(id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("agent {id}")))?;
        let published = agent.publish();
        self.upsert_agent(published).await
    }

    /// Create a new draft revision from a published (or any) agent.
    pub async fn revise_agent(&self, id: &AgentId) -> RouterResult<Agent> {
        let agent = self
            .agents
            .get(id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("agent {id}")))?;
        let draft = agent.revise();
        self.upsert_agent(draft).await
    }

    /// Upsert handler + persist.
    pub async fn upsert_handler(&self, handler: Handler) -> RouterResult<()> {
        self.handlers.upsert(handler.clone()).await?;
        self.store.save_handler(&handler).await?;
        Ok(())
    }

    /// Delete handler.
    pub async fn delete_handler(&self, id: &HandlerId) -> RouterResult<()> {
        self.handlers.remove(id).await?;
        self.store.delete_handler(id).await?;
        Ok(())
    }

    /// Upsert schedule + persist.
    pub async fn upsert_schedule(&self, schedule: Schedule) -> RouterResult<()> {
        self.scheduler.upsert(schedule.clone()).await?;
        self.store.save_schedule(&schedule).await?;
        Ok(())
    }

    /// Upsert webhook sink target + persist.
    pub async fn upsert_sink_target(&self, target: SinkTarget) -> RouterResult<SinkTarget> {
        let target = self.sinks.upsert_target(target).await;
        self.store.save_sink_target(&target).await?;
        Ok(target)
    }

    /// Delete webhook sink target.
    pub async fn delete_sink_target(&self, id: &str) -> RouterResult<()> {
        if !self.sinks.remove_target(id).await {
            return Err(RouterError::NotFound(format!("sink target {id}")));
        }
        self.store.delete_sink_target(id).await?;
        Ok(())
    }

    /// Upsert test case + persist.
    pub async fn upsert_test_case(&self, case: TestCase) -> RouterResult<TestCase> {
        let case = self.tests.upsert_case(case).await?;
        self.store.save_test_case(&case).await?;
        Ok(case)
    }

    /// Delete test case.
    pub async fn delete_test_case(&self, id: &str) -> RouterResult<()> {
        self.tests.remove_case(id).await?;
        self.store.delete_test_case(id).await?;
        Ok(())
    }

    /// Upsert dataset + persist.
    pub async fn upsert_dataset(&self, dataset: Dataset) -> RouterResult<Dataset> {
        let dataset = self.tests.upsert_dataset(dataset).await?;
        self.store.save_dataset(&dataset).await?;
        Ok(dataset)
    }

    /// Emit event, persist, fan-out sinks, dispatch handlers.
    pub async fn emit(&self, event: Event) -> RouterResult<Vec<AgentRun>> {
        let event = self.bus.emit(event).await?;
        if self.config.persist_events {
            self.store.save_event(&event).await?;
        }

        // Best-effort outbound adapters (webhook sink, …).
        self.sinks.deliver_all(&event).await;

        let matched = self.handlers.match_event(&event).await;
        let mut runs = Vec::new();
        for m in matched {
            match self.dispatch_handler(&m.handler, &m.event).await {
                Ok(mut produced) => runs.append(&mut produced),
                Err(err) => {
                    tracing::warn!(
                        handler = %m.handler.id,
                        error = %err,
                        "handler dispatch failed"
                    );
                    if matches!(m.handler.error_policy, ErrorPolicy::EmitFailedEvent) {
                        let fail = Event::new(
                            "handler.failed",
                            "routerai",
                            json!({
                                "handler_id": m.handler.id.to_string(),
                                "error": err.to_string(),
                            }),
                        )
                        .cause_of(&m.event);
                        let _ = self.emit_bus_only(fail).await;
                    }
                    if matches!(m.handler.error_policy, ErrorPolicy::FailFast) {
                        return Err(err);
                    }
                }
            }
        }
        Ok(runs)
    }

    /// Emit to bus (+ sinks) without re-entering handler dispatch (avoids loops).
    async fn emit_bus_only(&self, event: Event) -> RouterResult<Event> {
        let event = self.bus.emit(event).await?;
        if self.config.persist_events {
            self.store.save_event(&event).await?;
        }
        self.sinks.deliver_all(&event).await;
        Ok(event)
    }

    async fn dispatch_handler(
        &self,
        handler: &Handler,
        event: &Event,
    ) -> RouterResult<Vec<AgentRun>> {
        let mut runs = Vec::new();
        for action in &handler.actions {
            match action {
                Action::AgentRun { agent_id } => {
                    let run = self
                        .start_run_with_mode(
                            agent_id,
                            event.payload.clone(),
                            Some(event.id.clone()),
                            RunMode::Automated,
                        )
                        .await?;
                    runs.push(run);
                }
                Action::EventEmit {
                    event_type,
                    payload,
                    source,
                } => {
                    let child = Event::new(
                        event_type.clone(),
                        source.clone(),
                        payload.clone().unwrap_or_else(|| event.payload.clone()),
                    )
                    .cause_of(event);
                    // Avoid infinite re-entry loops: emit to bus/store only, do not re-dispatch.
                    let child = self.bus.emit(child).await?;
                    if self.config.persist_events {
                        self.store.save_event(&child).await?;
                    }
                }
                Action::ToolCall { tool_id, input } => {
                    let _ = self.tools.execute(tool_id, input.clone(), &[]).await?;
                }
            }
        }
        Ok(runs)
    }

    /// Start and execute an agent run (automated gate).
    pub async fn start_run(
        &self,
        agent_id: &AgentId,
        input: Value,
        event_id: Option<crate::ids::EventId>,
    ) -> RouterResult<AgentRun> {
        self.start_run_with_mode(agent_id, input, event_id, RunMode::Automated)
            .await
    }

    /// Start run with explicit mode (playground / test lab use Interactive).
    pub async fn start_run_with_mode(
        &self,
        agent_id: &AgentId,
        input: Value,
        event_id: Option<crate::ids::EventId>,
        mode: RunMode,
    ) -> RouterResult<AgentRun> {
        if self.kill_switch() {
            return Err(RouterError::Forbidden("kill switch enabled".into()));
        }

        let agent = self
            .agents
            .get(agent_id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("agent {agent_id}")))?;
        agent.ensure_runnable_mode(mode)?;

        let mut run = AgentRun::new(agent.id.clone(), input, event_id);
        run.status = RunStatus::Running;
        run.push_step(RunStepKind::Event, format!("run started ({mode:?})"));
        self.store.save_run(&run).await?;

        let result = self.execute_agent(&agent, &mut run).await;
        match result {
            Ok(()) => {}
            Err(RouterError::Cancelled) => {
                run.cancel();
                run.push_step(RunStepKind::Error, "cancelled");
            }
            Err(RouterError::Timeout) => {
                run.fail(RouterError::Timeout.to_string());
                run.status = RunStatus::TimedOut;
                run.push_step(RunStepKind::Error, "run exceeded max_runtime_seconds");
            }
            Err(err) => {
                run.fail(err.to_string());
                run.push_step(RunStepKind::Error, err.to_string());
            }
        }

        self.store.save_run(&run).await?;

        let done_type = if run.status == RunStatus::Completed {
            "agent.completed"
        } else {
            "agent.failed"
        };
        let mut done = Event::new(
            done_type,
            "routerai",
            json!({
                "run_id": run.id.to_string(),
                "agent_id": run.agent_id.to_string(),
                "status": run.status,
                "cost": run.cost.to_string(),
                "mode": mode,
                "output": run.output,
            }),
        );
        if let Some(ref eid) = run.event_id {
            done.causation_id = Some(eid.to_string());
            if let Some(parent) = self.bus.get(eid).await {
                done.correlation_id = parent
                    .correlation_id
                    .clone()
                    .or_else(|| Some(parent.id.to_string()));
                if let Some(reply) = parent.metadata.extra.get("reply_to") {
                    done.metadata.extra.insert("reply_to".into(), reply.clone());
                }
            } else {
                done.correlation_id = Some(eid.to_string());
            }
        }
        let _ = self.emit_bus_only(done).await;

        Ok(run)
    }

    /// Playground turn: interactive run with conversation message.
    pub async fn playground_message(
        &self,
        agent_id: &AgentId,
        message: impl Into<String>,
        context: Option<Value>,
    ) -> RouterResult<AgentRun> {
        let mut input = context.unwrap_or_else(|| json!({}));
        if let Some(obj) = input.as_object_mut() {
            obj.insert("message".into(), Value::String(message.into()));
            obj.insert("playground".into(), Value::Bool(true));
        } else {
            input = json!({ "message": message.into(), "playground": true });
        }
        self.start_run_with_mode(agent_id, input, None, RunMode::Interactive)
            .await
    }

    /// Agent loop: model turn → tool calls (policy-checked) → tool results → next turn,
    /// until the model answers without tool calls or a limit is hit.
    async fn execute_agent(&self, agent: &Agent, run: &mut AgentRun) -> RouterResult<()> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(agent.limits.max_runtime_seconds.max(1));
        let max_steps = agent.limits.max_steps.max(1);

        // Run input is untrusted (webhooks, events): it may not invoke tools directly.
        if run.input.get("tools").is_some() {
            run.push_step(
                RunStepKind::Observe,
                "input.tools ignored — tools are invoked only by the model under agent policy",
            );
        }

        let message = extract_user_message(&run.input);
        let Some(ai) = self.ai.clone() else {
            // Offline / stub mode without AiClient
            run.push_step(RunStepKind::Observe, "no AiClient — stub response");
            run.complete(json!({
                "text": format!("[stub] processed: {message}"),
                "stub": true,
            }));
            return Ok(());
        };

        let (toolset, withheld) = AgentToolset::resolve(&self.tools, agent).await;
        for (tool_id, reason) in withheld {
            run.push_step(
                RunStepKind::Observe,
                format!("tool {tool_id} withheld: {reason}"),
            );
        }

        let mut messages = vec![
            Message::system(agent.instructions.clone()),
            Message::user(message),
        ];
        let mut tool_calls_total = 0usize;

        // Agent spend also counts toward its own scope (daily limit per agent lineage).
        let budget_scope = format!("agent:{}", agent.lineage_id);

        for turn in 1..=max_steps {
            self.ensure_run_active(run, deadline).await?;
            // Remaining run budget caps the next request's worst case *before* sending.
            let remaining = match agent.budget.max_run_cost {
                Some(max) if run.cost >= max => {
                    return Err(RouterError::BudgetExceeded(format!(
                        "run cost {} reached max_run_cost {max}",
                        run.cost
                    )));
                }
                Some(max) => Some(max - run.cost),
                None => None,
            };

            run.push_step(
                RunStepKind::Llm,
                format!(
                    "llm {} / {} (turn {turn})",
                    agent.model.provider, agent.model.model
                ),
            );

            // Correlates every physical attempt (retries / fallbacks) of this turn.
            let request_id = universal_ai::RequestId::new();
            let mut builder = ai
                .chat()
                .request_id(request_id)
                .provider(agent.model.provider.clone())
                .model(agent.model.model.clone())
                .messages(messages.iter().cloned())
                .tools(toolset.tools().iter().cloned())
                .budget_scope(budget_scope.clone(), agent.budget.max_daily_cost);
            if let Some(remaining) = remaining {
                builder = builder.max_cost(remaining);
            }
            if let Some(t) = agent.model.temperature {
                builder = builder.temperature(t);
            }
            if let Some(m) = agent.model.max_tokens {
                builder = builder.max_tokens(m);
            }

            // The run deadline cancels the in-flight request; cancellation settles the
            // attempt (reservation kept) before returning, so its charge is visible.
            let result = builder
                .cancel_on(tokio::time::sleep_until(deadline))
                .send()
                .await;
            let response = match result {
                Ok(r) => r,
                Err(err) => {
                    // Failed attempts may still have been charged (timeouts, broken
                    // responses, cancellation): count them toward the run.
                    let charged = logical_charge(&ai, &request_id);
                    if !charged.is_zero() {
                        run.add_model_usage(RunUsage::default(), RunCost { amount: charged });
                    }
                    if matches!(err, universal_ai::AiError::Cancelled) {
                        return Err(RouterError::Timeout);
                    }
                    if let Some(reason) = err.budget_reason() {
                        if let Some(step) = run.steps.last_mut() {
                            step.detail = Some(json!({
                                "model": format!("{}/{}", agent.model.provider, agent.model.model),
                                "turn": turn,
                                "budget_decision": "rejected",
                                "reason": reason,
                                "remaining_run_budget": remaining,
                                "error": err.to_string(),
                            }));
                        }
                    }
                    let fail = Event::new(
                        "ai.request.failed",
                        "universal-ai",
                        json!({
                            "provider": agent.model.provider,
                            "model": agent.model.model,
                            "run_id": run.id.to_string(),
                            "agent_id": agent.id.to_string(),
                            "error": err.to_string(),
                            "budget_reason": err.budget_reason(),
                        }),
                    );
                    let _ = self.emit_bus_only(fail).await;
                    return Err(err.into());
                }
            };

            // Charged = actual cost when usage + pricing are known; otherwise the
            // worst-case reservation (budget-controlled) — never a silent zero.
            // Summed over every attempt of the turn (retries / fallbacks included).
            let accounting = ai.request_usage(&response.request_id);
            let charged = logical_charge(&ai, &request_id);
            run.add_model_usage(
                response.usage().map(RunUsage::from).unwrap_or_default(),
                RunCost { amount: charged },
            );

            let requested: Vec<&str> = response.tool_calls().iter().map(|c| c.name()).collect();
            if let Some(step) = run.steps.last_mut() {
                let a = accounting.as_ref().map(|a| &a.accounting);
                step.cost = Some(charged);
                step.detail = Some(json!({
                    "model": format!("{}/{}", agent.model.provider, agent.model.model),
                    "turn": turn,
                    "tool_calls": requested,
                    "request_id": response.request_id.to_string(),
                    "key_id": accounting.as_ref().and_then(|a| a.api_key.as_ref()).map(|k| k.to_string()),
                    "prompt_tokens": response.usage().map(|u| u.prompt_tokens),
                    "completion_tokens": response.usage().map(|u| u.completion_tokens),
                    "budget_decision": if a.and_then(|a| a.estimated_cost).is_some() && remaining.is_some() { "admitted" } else { "not_limited_by_run" },
                    "remaining_run_budget": remaining,
                    "estimated_cost": a.and_then(|a| a.estimated_cost),
                    "actual_cost": response.cost().map(|c| c.amount),
                    "charged_cost": charged,
                    "cost_status": a.map(|a| a.status),
                }));
            }

            let completed = Event::new(
                "ai.request.completed",
                "universal-ai",
                json!({
                    "provider": agent.model.provider,
                    "model": agent.model.model,
                    "run_id": run.id.to_string(),
                    "agent_id": agent.id.to_string(),
                    "prompt_tokens": response.usage().map(|u| u.prompt_tokens),
                    "completion_tokens": response.usage().map(|u| u.completion_tokens),
                    "total_tokens": response.usage().map(|u| u.total_tokens),
                    "cost": charged.to_string(),
                    "cost_status": accounting.as_ref().map(|a| a.accounting.status),
                }),
            );
            let _ = self.emit_bus_only(completed).await;

            // The gate admitted a worst case within the remaining budget; actual > estimate
            // would mean the provider billed beyond max_tokens — stop the run.
            if let Some(max) = agent.budget.max_run_cost {
                if run.cost > max {
                    return Err(RouterError::BudgetExceeded(format!(
                        "actual cost {} exceeded max_run_cost {max} beyond the pre-flight estimate",
                        run.cost
                    )));
                }
            }

            let calls = response.tool_calls().to_vec();
            if calls.is_empty() {
                let text = response.text();
                run.push_step(RunStepKind::Result, "agent completed");
                run.complete(json!({
                    "text": text,
                    "model": agent.model.model,
                    "provider": agent.model.provider,
                    "turns": turn,
                    "tool_calls": tool_calls_total,
                }));
                return Ok(());
            }

            // Every call gets a result so the conversation stays valid for the provider.
            messages.push(response.message);
            for (i, call) in calls.iter().enumerate() {
                self.ensure_run_active(run, deadline).await?;
                let result = if i >= MAX_TOOL_CALLS_PER_TURN {
                    run.push_step(
                        RunStepKind::Error,
                        format!(
                            "tool call {} skipped: per-turn tool call limit",
                            call.name()
                        ),
                    );
                    ToolResult::error(
                        call.id.clone(),
                        tool_error_payload(&format!(
                            "too many tool calls in one turn (max {MAX_TOOL_CALLS_PER_TURN})"
                        )),
                    )
                } else {
                    tool_calls_total += 1;
                    self.run_tool_call(agent, &toolset, call, run, deadline)
                        .await?
                };
                messages.push(Message::tool_result(result));
            }
        }

        Err(RouterError::Policy(format!(
            "agent did not produce a final answer within max_steps ({max_steps})"
        )))
    }

    /// Execute one model-requested call. Policy / argument / tool failures become an
    /// error tool result so the model can correct itself; only timeout aborts the run.
    async fn run_tool_call(
        &self,
        agent: &Agent,
        toolset: &AgentToolset,
        call: &ToolCall,
        run: &mut AgentRun,
        deadline: tokio::time::Instant,
    ) -> RouterResult<ToolResult> {
        let Some(tool_id) = toolset.tool_id(call.name()) else {
            run.push_step(
                RunStepKind::Error,
                format!(
                    "tool call rejected: '{}' is not available to this agent",
                    call.name()
                ),
            );
            return Ok(ToolResult::error(
                call.id.clone(),
                tool_error_payload(&format!(
                    "unknown tool '{}' — use one of the provided tools",
                    call.name()
                )),
            ));
        };

        let input = match call.arguments_json() {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                run.push_step(
                    RunStepKind::Error,
                    format!("tool {tool_id} invalid arguments: not a JSON object"),
                );
                return Ok(ToolResult::error(
                    call.id.clone(),
                    tool_error_payload("arguments must be a JSON object"),
                ));
            }
            Err(err) => {
                run.push_step(
                    RunStepKind::Error,
                    format!("tool {tool_id} invalid arguments: {err}"),
                );
                return Ok(ToolResult::error(
                    call.id.clone(),
                    tool_error_payload(&format!("arguments are not valid JSON: {err}")),
                ));
            }
        };

        run.push_step(RunStepKind::Tool, format!("tool {tool_id}"));
        let started = Instant::now();
        let outcome = tokio::time::timeout_at(
            deadline,
            self.tools.execute_for_agent(tool_id, input.clone(), agent),
        )
        .await
        .map_err(|_| RouterError::Timeout)?;
        let duration_ms = started.elapsed().as_millis() as u64;

        let (result, detail) = match outcome {
            Ok(output) => (
                ToolResult::success(call.id.clone(), output.to_string()),
                json!({ "output": output }),
            ),
            Err(err) => (
                ToolResult::error(call.id.clone(), tool_error_payload(&err.to_string())),
                json!({ "error": err.to_string() }),
            ),
        };
        if let Some(step) = run.steps.last_mut() {
            let mut d = json!({
                "tool": tool_id,
                "call_id": call.id,
                "input": input,
                "duration_ms": duration_ms,
                "is_error": result.is_error,
            });
            if let (Some(d), Some(extra)) = (d.as_object_mut(), detail.as_object()) {
                d.extend(extra.clone());
            }
            step.detail = Some(d);
        }
        if result.is_error {
            run.push_step(
                RunStepKind::Error,
                format!(
                    "tool {tool_id} failed: {}",
                    detail["error"].as_str().unwrap_or("")
                ),
            );
        } else {
            run.push_step(RunStepKind::Observe, format!("tool {tool_id} ok"));
        }
        Ok(result)
    }

    async fn ensure_run_active(
        &self,
        run: &AgentRun,
        deadline: tokio::time::Instant,
    ) -> RouterResult<()> {
        if self.is_cancelled(&run.id).await {
            return Err(RouterError::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RouterError::Timeout);
        }
        Ok(())
    }

    /// Cancel a run (cooperative).
    pub async fn cancel_run(&self, id: &RunId) -> RouterResult<()> {
        self.cancelled.write().await.insert(id.to_string());
        if let Some(mut run) = self.store.get_run(id).await? {
            if matches!(run.status, RunStatus::Pending | RunStatus::Running) {
                run.cancel();
                self.store.save_run(&run).await?;
            }
        }
        Ok(())
    }

    async fn is_cancelled(&self, id: &RunId) -> bool {
        self.cancelled.read().await.contains(id.as_str())
    }

    /// Retry a failed run with same input.
    pub async fn retry_run(&self, id: &RunId) -> RouterResult<AgentRun> {
        let prev = self
            .store
            .get_run(id)
            .await?
            .ok_or_else(|| RouterError::NotFound(format!("run {id}")))?;
        self.start_run(&prev.agent_id, prev.input, prev.event_id)
            .await
    }

    /// Run test case through agent + evaluate (interactive — drafts allowed).
    pub async fn run_test(&self, case: &TestCase) -> RouterResult<(AgentRun, EvaluationReport)> {
        let run = self
            .start_run_with_mode(
                &case.agent_id,
                case.input.clone(),
                None,
                RunMode::Interactive,
            )
            .await?;
        let report = TestLab::evaluate(case, &run);
        Ok((run, report))
    }

    /// Process due schedules once.
    pub async fn tick_schedules(&self) -> RouterResult<Vec<AgentRun>> {
        let now = Utc::now();
        let due = self.scheduler.due(now).await;
        let mut runs = Vec::new();
        for item in due {
            if let Some(updated) = self.scheduler.mark_fired(&item.schedule.id, now).await {
                self.store.save_schedule(&updated).await?;
            }
            let event = Event::new(
                "schedule.fired",
                "scheduler",
                json!({
                    "schedule_id": item.schedule.id.to_string(),
                    "name": item.schedule.name,
                }),
            );
            let _ = self.emit_bus_only(event.clone()).await;
            match &item.schedule.action {
                Action::AgentRun { agent_id } => {
                    let run = self
                        .start_run(agent_id, event.payload.clone(), Some(event.id))
                        .await?;
                    runs.push(run);
                }
                Action::EventEmit {
                    event_type,
                    payload,
                    source,
                } => {
                    let e = Event::new(
                        event_type.clone(),
                        source.clone(),
                        payload.clone().unwrap_or(json!({})),
                    );
                    let nested = self.emit(e).await?;
                    runs.extend(nested);
                }
                Action::ToolCall { tool_id, input } => {
                    let _ = self.tools.execute(tool_id, input.clone(), &[]).await?;
                }
            }
        }
        Ok(runs)
    }

    /// Run regression suite for a dataset (factual aggregates only).
    pub async fn run_regression(&self, dataset_id: &str) -> RouterResult<RegressionReport> {
        let ds = self
            .tests
            .get_dataset(dataset_id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("dataset {dataset_id}")))?;
        let mut pairs = Vec::new();
        for case_id in &ds.case_ids {
            let case = self
                .tests
                .get_case(case_id)
                .await
                .ok_or_else(|| RouterError::NotFound(format!("test case {case_id}")))?;
            let (run, report) = self.run_test(&case).await?;
            pairs.push((run, report));
        }
        let report = TestLab::aggregate_regression(dataset_id, ds.agent_id.clone(), &pairs);
        if let Some(mut agent) = self.agents.get(&ds.agent_id).await {
            agent.last_regression_passed = Some(report.failed == 0 && report.total > 0);
            agent.updated_at = Utc::now();
            let _ = self.upsert_agent(agent).await;
        }
        Ok(report)
    }

    /// Dashboard snapshot (factual counts / costs).
    pub async fn dashboard(&self) -> RouterResult<DashboardSnapshot> {
        let runs = self.store.list_runs(500).await?;
        let today = Utc::now().date_naive();
        let mut active = 0u64;
        let mut failed_today = 0u64;
        let mut cost_today = Decimal::ZERO;
        for r in &runs {
            if matches!(r.status, RunStatus::Running | RunStatus::Pending) {
                active += 1;
            }
            if r.started_at.date_naive() == today {
                cost_today += r.cost;
                if r.status == RunStatus::Failed {
                    failed_today += 1;
                }
            }
        }
        let events_hour = self
            .bus
            .list(10_000)
            .await
            .into_iter()
            .filter(|e| (Utc::now() - e.timestamp).num_minutes() < 60)
            .count() as u64;

        let agents = self.agents.list_latest().await;
        Ok(DashboardSnapshot {
            active_runs: active,
            events_last_hour: events_hour,
            cost_today,
            failed_runs_today: failed_today,
            agents: agents
                .into_iter()
                .map(|a| AgentDashRow {
                    id: a.id.to_string(),
                    name: a.name,
                    status: format!("{:?}", a.status).to_ascii_lowercase(),
                    version: a.version,
                })
                .collect(),
            kill_switch: self.kill_switch(),
            checked_at: Utc::now(),
        })
    }

    /// Health snapshot for `routerai doctor`.
    pub async fn doctor(&self) -> DoctorReport {
        DoctorReport {
            runtime_ok: !self.kill_switch(),
            kill_switch: self.kill_switch(),
            agents: self.agents.list().await.len(),
            handlers: self.handlers.list().await.len(),
            tools: self.tools.registry().list().await.len(),
            schedules: self.scheduler.list().await.len(),
            ai_configured: self.ai.is_some(),
            event_bus_ok: true,
            checked_at: Utc::now(),
        }
    }
}

/// Upper bound on tool calls executed from a single model response.
const MAX_TOOL_CALLS_PER_TURN: usize = 16;

/// Error content returned to the model as a tool result.
fn tool_error_payload(message: &str) -> String {
    json!({ "error": message }).to_string()
}

fn extract_user_message(input: &Value) -> String {
    input
        .get("message")
        .or_else(|| input.get("text"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string())
}

/// Doctor output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    /// Runtime accepting work.
    pub runtime_ok: bool,
    /// Kill switch.
    pub kill_switch: bool,
    /// Agent count.
    pub agents: usize,
    /// Handler count.
    pub handlers: usize,
    /// Tool count.
    pub tools: usize,
    /// Schedule count.
    pub schedules: usize,
    /// AiClient present.
    pub ai_configured: bool,
    /// Event bus.
    pub event_bus_ok: bool,
    /// Timestamp.
    pub checked_at: chrono::DateTime<Utc>,
}

/// Dashboard row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentDashRow {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Status string.
    pub status: String,
    /// Version.
    pub version: u32,
}

/// Dashboard snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DashboardSnapshot {
    /// Active runs.
    pub active_runs: u64,
    /// Events in last hour.
    pub events_last_hour: u64,
    /// Cost today.
    pub cost_today: Decimal,
    /// Failed today.
    pub failed_runs_today: u64,
    /// Agents.
    pub agents: Vec<AgentDashRow>,
    /// Kill switch.
    pub kill_switch: bool,
    /// Timestamp.
    pub checked_at: chrono::DateTime<Utc>,
}

/// Re-export for callers creating published agents quickly.
pub fn published_agent(name: &str, instructions: &str) -> Agent {
    let mut a = Agent::new(name, instructions);
    a.status = AgentStatus::Published;
    a.tools = vec!["json.echo".into(), "web.search".into(), "event.emit".into()];
    a
}

/// What one logical model request (all its physical attempts) was charged.
fn logical_charge(ai: &universal_ai::AiClient, request_id: &universal_ai::RequestId) -> Decimal {
    ai.logical_request_attempts(request_id)
        .iter()
        .map(universal_ai::RequestUsage::budget_charge)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::Handler;
    use serde_json::json;

    #[tokio::test]
    async fn event_to_handler_to_agent_stub() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        let agent = published_agent("sales", "You are a sales agent.");
        let agent_id = agent.id.clone();
        rt.upsert_agent(agent).await.unwrap();

        let handler = Handler::agent_on_event(
            "Sales handler",
            "telegram.message.received",
            agent_id.clone(),
        );
        rt.upsert_handler(handler).await.unwrap();

        let event = Event::new(
            "telegram.message.received",
            "telegram",
            json!({"text": "Хочу купить продукт", "chat_id": 123}),
        );
        let runs = rt.emit(event).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Completed);
        assert!(runs[0]
            .output
            .as_ref()
            .unwrap()
            .get("text")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("stub"));
    }

    #[tokio::test]
    async fn kill_switch_blocks_runs() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        rt.set_kill_switch(true);
        let agent = published_agent("x", "y");
        let id = agent.id.clone();
        rt.upsert_agent(agent).await.unwrap();
        let err = rt.start_run(&id, json!({}), None).await.unwrap_err();
        assert!(matches!(err, RouterError::Forbidden(_)));
    }

    #[tokio::test]
    async fn publish_and_revise_versions() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        let draft = Agent::new("sales", "v1");
        let id = draft.id.clone();
        rt.upsert_agent(draft).await.unwrap();
        let pubd = rt.publish_agent(&id).await.unwrap();
        assert_eq!(pubd.status, AgentStatus::Published);
        let v2 = rt.revise_agent(&id).await.unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.status, AgentStatus::Draft);
        assert_eq!(v2.lineage_id, pubd.lineage_id);
    }

    #[tokio::test]
    async fn draft_interactive_ok_automated_blocked() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        let draft = Agent::new("sales", "help");
        let id = draft.id.clone();
        rt.upsert_agent(draft).await.unwrap();
        let interactive = rt
            .start_run_with_mode(&id, json!({"message": "hi"}), None, RunMode::Interactive)
            .await
            .unwrap();
        assert_eq!(interactive.status, RunStatus::Completed);
        let err = rt.start_run(&id, json!({}), None).await.unwrap_err();
        assert!(matches!(err, RouterError::Forbidden(_)));
    }

    #[tokio::test]
    async fn control_plane_publish_with_validation() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        let mut draft = Agent::new("sales", "You sell widgets.");
        draft.tools = vec!["json.echo".into()];
        let id = draft.id.clone();
        rt.upsert_agent(draft).await.unwrap();

        let result = rt
            .control()
            .publish(
                &id,
                crate::control::PublishOptions {
                    override_tests: false,
                    actor: "test".into(),
                },
            )
            .await
            .unwrap();
        assert!(result.agent.is_some());
        assert_eq!(result.agent.unwrap().status, AgentStatus::Published);

        let audit = rt.audit().list(10).await;
        assert!(audit.iter().any(|e| e.action == "agent.publish"));
    }

    #[tokio::test]
    async fn control_plane_pause_resume() {
        let rt = RouterRuntime::builder().build().await.unwrap();
        let agent = published_agent("x", "y");
        let id = agent.id.clone();
        rt.upsert_agent(agent).await.unwrap();
        let paused = rt.control().pause(&id, "ops").await.unwrap();
        assert_eq!(paused.status, AgentStatus::Paused);
        let err = rt.start_run(&id, json!({}), None).await.unwrap_err();
        assert!(matches!(err, RouterError::Forbidden(_)));
        let resumed = rt.control().resume(&id, "ops").await.unwrap();
        assert_eq!(resumed.status, AgentStatus::Published);
    }
}
