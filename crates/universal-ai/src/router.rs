//! Deterministic model / provider router.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::budget::worst_case_cost;
use crate::capability::Capability;
use crate::cost::CostManager;
use crate::error::{AiError, AiResult};
use crate::health::HealthMonitor;
use crate::models::ModelRegistry;
use crate::provider::DynProvider;
use crate::types::{ChatRequest, ModelId, ProviderId};

/// High-level task class for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Plain text generation / chat.
    TextGeneration,
    /// Embeddings.
    Embeddings,
    /// Reasoning-heavy.
    Reasoning,
    /// Tool-calling agent turn.
    ToolUse,
}

/// Max cost constraint.
#[derive(Debug, Clone)]
pub struct MaxCost {
    /// Amount in USD.
    pub amount: Decimal,
}

impl MaxCost {
    /// Parse USD decimal string.
    pub fn usd(value: &str) -> AiResult<Self> {
        let amount = value.parse().map_err(|e| AiError::InvalidRequest {
            message: format!("invalid cost: {e}"),
        })?;
        Ok(Self { amount })
    }
}

/// Multi-key selection strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySelectionStrategy {
    /// First active key.
    FirstAvailable,
    /// Round-robin.
    RoundRobin,
    /// Least request count.
    LeastUsed,
    /// Lowest accumulated cost.
    LowestCost,
    /// Prefer highest balance account (best-effort).
    HighestBalance,
}

/// Deterministic policy router.
pub struct Router {
    providers: Vec<DynProvider>,
    models: Arc<ModelRegistry>,
    cost: Arc<CostManager>,
    health: Arc<HealthMonitor>,
    task: Option<TaskType>,
    max_cost: Option<Decimal>,
    max_latency: Option<Duration>,
}

impl Router {
    /// Create.
    pub fn new(
        providers: Vec<DynProvider>,
        models: Arc<ModelRegistry>,
        cost: Arc<CostManager>,
        health: Arc<HealthMonitor>,
    ) -> Self {
        Self {
            providers,
            models,
            cost,
            health,
            task: None,
            max_cost: None,
            max_latency: None,
        }
    }

    /// Set task type.
    pub fn task(mut self, task: TaskType) -> Self {
        self.task = Some(task);
        self
    }

    /// Budget cap.
    pub fn budget(mut self, max: MaxCost) -> Self {
        self.max_cost = Some(max.amount);
        self
    }

    /// Alias.
    pub fn max_cost(mut self, amount: Decimal) -> Self {
        self.max_cost = Some(amount);
        self
    }

    /// Latency preference (soft — filters by last health latency when known).
    pub fn max_latency(mut self, d: Duration) -> Self {
        self.max_latency = Some(d);
        self
    }

    /// Routing decision for `request`: eligible `(provider, model)` pairs in
    /// order. The router never sends anything — execute the request with
    /// [`crate::AiClient::chat`] (e.g. `.provider(id)`), so every attempt passes the
    /// budget gate and is accounted.
    ///
    /// With a cost cap ([`Router::budget`] / [`Router::max_cost`]) a provider is
    /// only eligible when the request's worst case is known and fits: unknown
    /// pricing or an unbounded output excludes it (fail-closed).
    pub async fn select(&self, request: &ChatRequest) -> AiResult<Vec<(ProviderId, ModelId)>> {
        Ok(self
            .candidates_for(request)
            .await?
            .into_iter()
            .map(|(p, m)| (p.id(), m))
            .collect())
    }

    async fn candidates_for(&self, request: &ChatRequest) -> AiResult<Vec<(DynProvider, ModelId)>> {
        let needed = match self.task.unwrap_or(TaskType::TextGeneration) {
            TaskType::TextGeneration => Capability::Chat,
            TaskType::Embeddings => Capability::Embeddings,
            TaskType::Reasoning => Capability::Reasoning,
            TaskType::ToolUse => Capability::ToolCalling,
        };

        let mut out = Vec::new();
        for provider in &self.providers {
            if !provider.supports(needed) {
                continue;
            }
            if !self.health.is_healthy(&provider.id()).await {
                continue;
            }
            if let Some(max_lat) = self.max_latency {
                if let Some(status) = self.health.get(&provider.id()).await {
                    if let Some(lat) = status.latency_ms {
                        if Duration::from_millis(lat) > max_lat {
                            continue;
                        }
                    }
                }
            }
            if let Some(max) = self.max_cost {
                match worst_case_cost(&self.cost, &self.models, &provider.id(), request) {
                    Ok(est) if est.total <= max => {}
                    _ => continue,
                }
            }
            out.push((Arc::clone(provider), request.model.clone()));
        }

        if out.is_empty() {
            return Err(AiError::NoAvailableProvider {
                message: format!(
                    "no healthy provider for task={:?} model={}",
                    self.task, request.model
                ),
            });
        }
        Ok(out)
    }

    /// Resolve which registered provider owns a model id.
    pub fn resolve_provider(&self, model: &str) -> AiResult<ProviderId> {
        if let Some(info) = self.models.get(model) {
            return Ok(info.provider);
        }
        // Heuristic prefixes
        let lower = model.to_ascii_lowercase();
        if lower.starts_with("gpt") || lower.starts_with("o1") || lower.starts_with("o3") {
            return Ok(ProviderId::openai());
        }
        if lower.starts_with("deepseek") {
            return Ok(ProviderId::deepseek());
        }
        if lower.starts_with("claude") {
            return Ok(ProviderId::anthropic());
        }
        if lower.starts_with("gemini") {
            return Ok(ProviderId::gemini());
        }
        if lower.starts_with("grok") {
            return Ok(ProviderId::xai());
        }
        // Prefer single configured provider
        if self.providers.len() == 1 {
            return Ok(self.providers[0].id());
        }
        Err(AiError::UnsupportedModel {
            model: model.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_cost_parses() {
        let c = MaxCost::usd("0.01").unwrap();
        assert_eq!(c.amount, Decimal::new(1, 2));
    }
}
