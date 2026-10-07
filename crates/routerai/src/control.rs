//! Control plane — manage agent lifecycle, not execute model calls.

use serde::{Deserialize, Serialize};

use crate::agent::{Agent, AgentStatus};
use crate::audit::AuditLog;
use crate::error::{RouterError, RouterResult};
use crate::ids::AgentId;
use crate::lifecycle::{self, PublishValidationReport};
use crate::runtime::RouterRuntime;

/// Publish request options.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PublishOptions {
    /// Allow publish despite soft test failures.
    #[serde(default)]
    pub override_tests: bool,
    /// Actor for audit.
    #[serde(default = "default_actor")]
    pub actor: String,
}

fn default_actor() -> String {
    "console".into()
}

/// Result of publish with validation.
#[derive(Debug, Clone, Serialize)]
pub struct PublishResult {
    /// Published agent (if successful).
    pub agent: Option<Agent>,
    /// Validation report.
    pub validation: PublishValidationReport,
    /// Whether override was used.
    pub override_used: bool,
}

/// Control plane facade over [`RouterRuntime`].
pub struct ControlPlane<'a> {
    runtime: &'a RouterRuntime,
}

impl<'a> ControlPlane<'a> {
    /// Borrow runtime.
    pub fn new(runtime: &'a RouterRuntime) -> Self {
        Self { runtime }
    }

    /// Access audit log.
    pub fn audit(&self) -> &AuditLog {
        self.runtime.audit()
    }

    /// Validate without publishing.
    pub async fn validate_publish(&self, id: &AgentId) -> RouterResult<PublishValidationReport> {
        self.runtime.validate_publish(id).await
    }

    /// Publish with validation (+ optional test override → audit).
    pub async fn publish(&self, id: &AgentId, opts: PublishOptions) -> RouterResult<PublishResult> {
        let report = self.runtime.validate_publish(id).await?;
        let test_soft_fail = report
            .soft_failures()
            .iter()
            .any(|c| c.id.starts_with("tests."));
        let hard_fail = !report.blocking_failures().is_empty();

        if hard_fail {
            self.audit()
                .record(
                    "agent.publish.rejected",
                    &opts.actor,
                    Some(id.to_string()),
                    serde_json::json!({ "validation": report }),
                )
                .await;
            return Ok(PublishResult {
                agent: None,
                validation: report,
                override_used: false,
            });
        }

        if test_soft_fail && !opts.override_tests {
            self.audit()
                .record(
                    "agent.publish.blocked_tests",
                    &opts.actor,
                    Some(id.to_string()),
                    serde_json::json!({ "validation": report }),
                )
                .await;
            return Err(RouterError::Policy(
                "publish blocked by test/regression checks — set override_tests=true to force"
                    .into(),
            ));
        }

        let override_used = test_soft_fail && opts.override_tests;
        if override_used {
            self.audit()
                .record(
                    "agent.publish.override_tests",
                    &opts.actor,
                    Some(id.to_string()),
                    serde_json::json!({ "validation": report }),
                )
                .await;
        }

        let agent = self.runtime.publish_agent(id).await?;
        self.audit()
            .record(
                "agent.publish",
                &opts.actor,
                Some(agent.id.to_string()),
                serde_json::json!({
                    "version": agent.version,
                    "override_used": override_used,
                }),
            )
            .await;

        Ok(PublishResult {
            agent: Some(agent),
            validation: report,
            override_used,
        })
    }

    /// Pause published agent.
    pub async fn pause(&self, id: &AgentId, actor: &str) -> RouterResult<Agent> {
        let agent = self.get(id).await?;
        let next = lifecycle::transition(&agent, AgentStatus::Paused)?;
        let saved = self.runtime.upsert_agent(next).await?;
        self.audit()
            .record(
                "agent.pause",
                actor,
                Some(id.to_string()),
                serde_json::json!({}),
            )
            .await;
        Ok(saved)
    }

    /// Resume paused → published.
    pub async fn resume(&self, id: &AgentId, actor: &str) -> RouterResult<Agent> {
        let agent = self.get(id).await?;
        let next = lifecycle::transition(&agent, AgentStatus::Published)?;
        let saved = self.runtime.upsert_agent(next).await?;
        self.audit()
            .record(
                "agent.resume",
                actor,
                Some(id.to_string()),
                serde_json::json!({}),
            )
            .await;
        Ok(saved)
    }

    /// Archive.
    pub async fn archive(&self, id: &AgentId, actor: &str) -> RouterResult<Agent> {
        let agent = self.get(id).await?;
        let next = lifecycle::transition(&agent, AgentStatus::Archived)?;
        let saved = self.runtime.upsert_agent(next).await?;
        self.audit()
            .record(
                "agent.archive",
                actor,
                Some(id.to_string()),
                serde_json::json!({}),
            )
            .await;
        Ok(saved)
    }

    /// Mark testing.
    pub async fn mark_testing(&self, id: &AgentId, actor: &str) -> RouterResult<Agent> {
        let agent = self.get(id).await?;
        let next = lifecycle::transition(&agent, AgentStatus::Testing)?;
        let saved = self.runtime.upsert_agent(next).await?;
        self.audit()
            .record(
                "agent.testing",
                actor,
                Some(id.to_string()),
                serde_json::json!({}),
            )
            .await;
        Ok(saved)
    }

    /// Mark ready.
    pub async fn mark_ready(&self, id: &AgentId, actor: &str) -> RouterResult<Agent> {
        let agent = self.get(id).await?;
        let next = lifecycle::transition(&agent, AgentStatus::Ready)?;
        let saved = self.runtime.upsert_agent(next).await?;
        self.audit()
            .record(
                "agent.ready",
                actor,
                Some(id.to_string()),
                serde_json::json!({}),
            )
            .await;
        Ok(saved)
    }

    /// Rollback lineage to a previous published version (re-publish that version record).
    pub async fn rollback(
        &self,
        lineage_or_id: &AgentId,
        to_version: u32,
        actor: &str,
    ) -> RouterResult<Agent> {
        let current = self.get(lineage_or_id).await?;
        let versions = self.runtime.agents().versions(&current.lineage_id).await;
        let target = versions
            .into_iter()
            .find(|v| v.version == to_version)
            .ok_or_else(|| {
                RouterError::NotFound(format!("version {to_version} not found in lineage"))
            })?;
        // Pause current published heads in lineage
        for v in self.runtime.agents().versions(&current.lineage_id).await {
            if v.status == AgentStatus::Published && v.id != target.id {
                let paused = lifecycle::transition(&v, AgentStatus::Paused)?;
                self.runtime.upsert_agent(paused).await?;
            }
        }
        let mut restored = target;
        restored.status = AgentStatus::Published;
        restored.updated_at = chrono::Utc::now();
        let saved = self.runtime.upsert_agent(restored).await?;
        self.audit()
            .record(
                "agent.rollback",
                actor,
                Some(saved.id.to_string()),
                serde_json::json!({ "to_version": to_version }),
            )
            .await;
        Ok(saved)
    }

    /// Global kill switch.
    pub async fn set_kill_switch(&self, on: bool, actor: &str) -> bool {
        if let Err(err) = self.runtime.set_kill_switch_persisted(on).await {
            tracing::warn!(error = %err, "failed to persist kill switch");
            self.runtime.set_kill_switch(on);
        }
        self.audit()
            .record(
                "runtime.kill_switch",
                actor,
                None,
                serde_json::json!({ "enabled": on }),
            )
            .await;
        on
    }

    async fn get(&self, id: &AgentId) -> RouterResult<Agent> {
        self.runtime
            .agents()
            .get(id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("agent {id}")))
    }
}
