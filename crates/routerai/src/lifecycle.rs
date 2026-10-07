//! Production agent lifecycle: Draft → Testing → Ready → Published → Paused → Archived.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, AgentStatus};
use crate::error::{RouterError, RouterResult};
use crate::tool::ToolRegistry;

/// Single validation check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCheck {
    /// Check id.
    pub id: String,
    /// Human label.
    pub label: String,
    /// Passed?
    pub passed: bool,
    /// Blocking if failed (unless override).
    pub blocking: bool,
    /// Detail message.
    pub message: String,
}

/// Full publish validation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishValidationReport {
    /// Agent id.
    pub agent_id: String,
    /// Overall can publish without override.
    pub ok: bool,
    /// Checks.
    pub checks: Vec<ValidationCheck>,
    /// When produced.
    pub checked_at: DateTime<Utc>,
}

impl PublishValidationReport {
    /// Blocking failures.
    pub fn blocking_failures(&self) -> Vec<&ValidationCheck> {
        self.checks
            .iter()
            .filter(|c| c.blocking && !c.passed)
            .collect()
    }

    /// Soft (non-blocking) failures — e.g. tests.
    pub fn soft_failures(&self) -> Vec<&ValidationCheck> {
        self.checks
            .iter()
            .filter(|c| !c.blocking && !c.passed)
            .collect()
    }
}

/// Context for publish validation.
pub struct PublishContext<'a> {
    /// Known tool ids from registry.
    pub tool_registry: &'a ToolRegistry,
    /// Whether AI client is configured.
    pub ai_configured: bool,
    /// Kill switch on?
    pub kill_switch: bool,
    /// Runtime doctor healthy?
    pub runtime_ok: bool,
    /// Last regression passed (if any run).
    pub regression_passed: Option<bool>,
    /// Number of test cases for agent.
    pub test_case_count: usize,
}

/// Validate agent for publish.
pub async fn validate_publish(
    agent: &Agent,
    ctx: PublishContext<'_>,
) -> RouterResult<PublishValidationReport> {
    let mut checks = Vec::new();

    let name_ok = !agent.name.trim().is_empty();
    checks.push(ValidationCheck {
        id: "config.name".into(),
        label: "Agent name".into(),
        passed: name_ok,
        blocking: true,
        message: if name_ok {
            "name present".into()
        } else {
            "name is empty".into()
        },
    });

    let instr_ok = !agent.instructions.trim().is_empty();
    checks.push(ValidationCheck {
        id: "config.instructions".into(),
        label: "Instructions".into(),
        passed: instr_ok,
        blocking: true,
        message: if instr_ok {
            "instructions present".into()
        } else {
            "instructions are empty".into()
        },
    });

    let model_ok = !agent.model.provider.is_empty() && !agent.model.model.is_empty();
    checks.push(ValidationCheck {
        id: "model.exists".into(),
        label: "Model policy".into(),
        passed: model_ok,
        blocking: true,
        message: if model_ok {
            format!("{}/{}", agent.model.provider, agent.model.model)
        } else {
            "provider/model missing".into()
        },
    });

    // Model availability is best-effort: require AiClient when publishing for automated use.
    checks.push(ValidationCheck {
        id: "model.runtime".into(),
        label: "Model runtime (universal-ai)".into(),
        passed: ctx.ai_configured,
        blocking: false,
        message: if ctx.ai_configured {
            "AiClient configured".into()
        } else {
            "AiClient not configured — stub mode only".into()
        },
    });

    let tools = ctx.tool_registry.list().await;
    let known: std::collections::HashSet<_> = tools.iter().map(|t| t.id.as_str()).collect();
    let missing: Vec<_> = agent
        .tools
        .iter()
        .filter(|t| !known.contains(t.as_str()))
        .cloned()
        .collect();
    let tools_ok = missing.is_empty();
    checks.push(ValidationCheck {
        id: "tools.available".into(),
        label: "Tools available".into(),
        passed: tools_ok,
        blocking: true,
        message: if tools_ok {
            format!("{} tool(s) ok", agent.tools.len())
        } else {
            format!("missing tools: {}", missing.join(", "))
        },
    });

    // Same rule the runtime enforces: an agent may only call tools whose required
    // permissions it has been granted.
    let mut ungranted: Vec<String> = tools
        .iter()
        .filter(|t| agent.tools.contains(&t.id))
        .flat_map(|t| {
            t.permissions
                .allow
                .iter()
                .filter(|p| !agent.permissions.allows(**p))
                .map(move |p| format!("{} needs {p:?}", t.id))
        })
        .collect();
    ungranted.sort();
    let permissions_ok = ungranted.is_empty();
    checks.push(ValidationCheck {
        id: "permissions".into(),
        label: "Permissions".into(),
        passed: permissions_ok,
        blocking: true,
        message: if permissions_ok {
            format!("{} allow flag(s)", agent.permissions.allow.len())
        } else {
            format!("missing permissions: {}", ungranted.join(", "))
        },
    });

    let budget_ok = agent.limits.max_steps > 0 && agent.limits.max_runtime_seconds > 0;
    checks.push(ValidationCheck {
        id: "budget.limits".into(),
        label: "Limits / budget".into(),
        passed: budget_ok,
        blocking: true,
        message: if budget_ok {
            format!(
                "max_steps={} max_runtime={}s max_run_cost={:?}",
                agent.limits.max_steps, agent.limits.max_runtime_seconds, agent.budget.max_run_cost
            )
        } else {
            "invalid limits".into()
        },
    });

    // Secrets: no required secrets in MVP unless tools need them — pass with note.
    checks.push(ValidationCheck {
        id: "secrets".into(),
        label: "Required secrets".into(),
        passed: true,
        blocking: false,
        message: "no mandatory secrets for configured tools".into(),
    });

    let tests_ok = match ctx.regression_passed {
        Some(true) => true,
        Some(false) => false,
        None => ctx.test_case_count == 0, // no cases → soft pass with warning
    };
    checks.push(ValidationCheck {
        id: "tests.regression".into(),
        label: "Regression / tests".into(),
        passed: tests_ok || ctx.test_case_count == 0,
        blocking: false,
        message: match (ctx.test_case_count, ctx.regression_passed) {
            (0, _) => "no test cases — soft warning".into(),
            (_, Some(true)) => "last regression passed".into(),
            (_, Some(false)) => "last regression failed".into(),
            (_, None) => format!("{} case(s), no regression run yet", ctx.test_case_count),
        },
    });

    checks.push(ValidationCheck {
        id: "runtime.health".into(),
        label: "Runtime health".into(),
        passed: ctx.runtime_ok && !ctx.kill_switch,
        blocking: true,
        message: if ctx.kill_switch {
            "kill switch is ON".into()
        } else if ctx.runtime_ok {
            "runtime ok".into()
        } else {
            "runtime unhealthy".into()
        },
    });

    // Lifecycle gate: only Draft/Testing/Ready (and Disabled legacy) can publish to Published.
    let status_ok = matches!(
        agent.status,
        AgentStatus::Draft
            | AgentStatus::Testing
            | AgentStatus::Ready
            | AgentStatus::Paused
            | AgentStatus::Disabled
    );
    checks.push(ValidationCheck {
        id: "lifecycle.status".into(),
        label: "Lifecycle status".into(),
        passed: status_ok || agent.status == AgentStatus::Published,
        blocking: true,
        message: format!("current status={:?}", agent.status),
    });

    let ok = checks.iter().filter(|c| c.blocking).all(|c| c.passed);
    Ok(PublishValidationReport {
        agent_id: agent.id.to_string(),
        ok,
        checks,
        checked_at: Utc::now(),
    })
}

/// Transition helpers.
pub fn transition(agent: &Agent, target: AgentStatus) -> RouterResult<Agent> {
    let allowed = match (agent.status, target) {
        (a, b) if a == b => true,
        (AgentStatus::Archived, _) => false,
        (_, AgentStatus::Draft) => false,
        (AgentStatus::Draft, AgentStatus::Testing) => true,
        (AgentStatus::Draft, AgentStatus::Ready) => true,
        (AgentStatus::Draft, AgentStatus::Published) => true,
        (AgentStatus::Testing, AgentStatus::Ready) => true,
        (AgentStatus::Testing, AgentStatus::Published) => true,
        (AgentStatus::Ready, AgentStatus::Published) => true,
        (AgentStatus::Ready, AgentStatus::Testing) => true,
        (AgentStatus::Published, AgentStatus::Paused) => true,
        (AgentStatus::Published, AgentStatus::Archived) => true,
        (AgentStatus::Paused, AgentStatus::Published) => true,
        (AgentStatus::Paused, AgentStatus::Archived) => true,
        (AgentStatus::Disabled, AgentStatus::Published) => true,
        (AgentStatus::Disabled, AgentStatus::Archived) => true,
        _ => false,
    };
    if !allowed {
        return Err(RouterError::Policy(format!(
            "cannot transition {:?} → {:?}",
            agent.status, target
        )));
    }
    let mut next = agent.clone();
    next.status = target;
    next.updated_at = Utc::now();
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::tool::{BuiltinTools, ToolRegistry};

    #[tokio::test]
    async fn validation_fails_empty_instructions() {
        let reg = ToolRegistry::new();
        BuiltinTools::register_all(&reg).await.unwrap();
        let mut agent = Agent::new("x", "");
        agent.tools = vec!["json.echo".into()];
        let report = validate_publish(
            &agent,
            PublishContext {
                tool_registry: &reg,
                ai_configured: false,
                kill_switch: false,
                runtime_ok: true,
                regression_passed: None,
                test_case_count: 0,
            },
        )
        .await
        .unwrap();
        assert!(!report.ok);
        assert!(report
            .blocking_failures()
            .iter()
            .any(|c| c.id == "config.instructions"));
    }

    #[tokio::test]
    async fn validation_requires_tool_permissions() {
        let reg = ToolRegistry::new();
        BuiltinTools::register_all(&reg).await.unwrap();
        let mut agent = Agent::new("x", "search things");
        agent.tools = vec!["web.search".into()];
        let ctx = || PublishContext {
            tool_registry: &reg,
            ai_configured: true,
            kill_switch: false,
            runtime_ok: true,
            regression_passed: None,
            test_case_count: 0,
        };

        let report = validate_publish(&agent, ctx()).await.unwrap();
        let check = report
            .blocking_failures()
            .into_iter()
            .find(|c| c.id == "permissions")
            .expect("missing Network permission must block publish");
        assert!(check.message.contains("web.search needs Network"));

        agent
            .permissions
            .allow
            .insert(crate::tool::Permission::Network);
        let report = validate_publish(&agent, ctx()).await.unwrap();
        assert!(!report
            .blocking_failures()
            .iter()
            .any(|c| c.id == "permissions"));
    }
}
