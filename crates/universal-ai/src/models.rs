//! Model registry.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::capability::ModelCapabilities;
use crate::error::{AiError, AiResult};
use crate::pricing::ModelPricing;
use crate::types::{ModelId, ProviderId};

/// Model metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model id.
    pub id: ModelId,
    /// Provider.
    pub provider: ProviderId,
    /// Display name.
    pub name: Option<String>,
    /// Context window.
    pub context_window: Option<u64>,
    /// Max output tokens.
    pub max_output_tokens: Option<u64>,
    /// Capabilities.
    pub capabilities: ModelCapabilities,
    /// Optional attached pricing snapshot.
    pub pricing: Option<ModelPricing>,
}

/// In-memory model registry.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    models: RwLock<HashMap<String, ModelInfo>>,
}

impl ModelRegistry {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert.
    pub fn upsert(&self, info: ModelInfo) {
        let key = format!("{}:{}", info.provider, info.id);
        if let Ok(mut g) = self.models.write() {
            g.insert(key, info);
        }
    }

    /// List all.
    pub fn list(&self) -> Vec<ModelInfo> {
        self.models
            .read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get by model id (first match) or provider:model.
    pub fn get(&self, model: &str) -> Option<ModelInfo> {
        let g = self.models.read().ok()?;
        if let Some(m) = g.get(model) {
            return Some(m.clone());
        }
        g.values().find(|m| m.id.as_str() == model).cloned()
    }

    /// Resolve provider for a model id.
    pub fn provider_for(&self, model: &str) -> AiResult<ProviderId> {
        self.get(model)
            .map(|m| m.provider)
            .ok_or_else(|| AiError::UnsupportedModel {
                model: model.to_string(),
            })
    }
}

/// Fluent models API surface used by AiClient.
pub struct ModelsApi<'a> {
    pub(crate) registry: &'a ModelRegistry,
}

impl ModelsApi<'_> {
    /// List registered models.
    pub async fn list(&self) -> AiResult<Vec<ModelInfo>> {
        Ok(self.registry.list())
    }

    /// Get one.
    pub async fn get(&self, model: &str) -> AiResult<Option<ModelInfo>> {
        Ok(self.registry.get(model))
    }

    /// Register / update model metadata. A known `max_output_tokens` lets budgeted
    /// requests without `max_tokens` be estimated (worst case) instead of rejected.
    pub fn register(&self, info: ModelInfo) {
        self.registry.upsert(info);
    }
}
