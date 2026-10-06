//! Pricing registry — prices live outside provider adapters.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::{AiError, AiResult};
use crate::types::{ModelId, ProviderId};

/// Price sheet entry for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Provider.
    pub provider: ProviderId,
    /// Model.
    pub model: ModelId,
    /// USD per 1M input tokens.
    pub input_per_million: Option<Decimal>,
    /// USD per 1M output tokens.
    pub output_per_million: Option<Decimal>,
    /// USD per 1M cached input tokens.
    pub cached_input_per_million: Option<Decimal>,
    /// When this price becomes effective.
    pub effective_from: DateTime<Utc>,
}

/// Async pricing source.
#[async_trait]
pub trait PricingProvider: Send + Sync {
    /// Lookup price.
    async fn get_price(
        &self,
        provider: ProviderId,
        model: ModelId,
    ) -> AiResult<Option<ModelPricing>>;
}

/// In-memory static table (still updatable at runtime).
#[derive(Debug, Default)]
pub struct StaticPricing {
    entries: RwLock<HashMap<(String, String), ModelPricing>>,
}

impl StaticPricing {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace.
    pub fn upsert(&self, pricing: ModelPricing) {
        let key = (pricing.provider.to_string(), pricing.model.to_string());
        if let Ok(mut g) = self.entries.write() {
            g.insert(key, pricing);
        }
    }

    /// Sync get.
    pub fn get(&self, provider: &ProviderId, model: &ModelId) -> Option<ModelPricing> {
        let key = (provider.to_string(), model.to_string());
        self.entries.read().ok()?.get(&key).cloned()
    }
}

#[async_trait]
impl PricingProvider for StaticPricing {
    async fn get_price(
        &self,
        provider: ProviderId,
        model: ModelId,
    ) -> AiResult<Option<ModelPricing>> {
        Ok(self.get(&provider, &model))
    }
}

/// Placeholder remote fetcher (URL + JSON map). Update without shipping a new binary.
#[derive(Debug)]
pub struct RemotePricing {
    /// Endpoint returning JSON array of [`ModelPricing`].
    pub url: String,
    cache: StaticPricing,
}

impl RemotePricing {
    /// Create unbound remote source.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            cache: StaticPricing::new(),
        }
    }

    /// Seed cache (tests / offline).
    pub fn seed(&self, pricing: ModelPricing) {
        self.cache.upsert(pricing);
    }
}

#[async_trait]
impl PricingProvider for RemotePricing {
    async fn get_price(
        &self,
        provider: ProviderId,
        model: ModelId,
    ) -> AiResult<Option<ModelPricing>> {
        // Cache-first; full HTTP refresh can be added by the host app.
        Ok(self.cache.get(&provider, &model))
    }
}

/// User-supplied callback pricing.
pub struct CustomPricing<F>
where
    F: Fn(&ProviderId, &ModelId) -> Option<ModelPricing> + Send + Sync,
{
    lookup: F,
}

impl<F> CustomPricing<F>
where
    F: Fn(&ProviderId, &ModelId) -> Option<ModelPricing> + Send + Sync,
{
    /// Wrap a function.
    pub fn new(lookup: F) -> Self {
        Self { lookup }
    }
}

#[async_trait]
impl<F> PricingProvider for CustomPricing<F>
where
    F: Fn(&ProviderId, &ModelId) -> Option<ModelPricing> + Send + Sync,
{
    async fn get_price(
        &self,
        provider: ProviderId,
        model: ModelId,
    ) -> AiResult<Option<ModelPricing>> {
        Ok((self.lookup)(&provider, &model))
    }
}

/// Composite registry used by CostManager.
#[derive(Debug, Default)]
pub struct PricingRegistry {
    static_table: StaticPricing,
}

impl PricingRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a static price (runtime-updatable; not baked into adapters).
    pub fn upsert(&self, pricing: ModelPricing) {
        self.static_table.upsert(pricing);
    }

    /// Sync lookup.
    pub fn get_price_sync(&self, provider: &ProviderId, model: &ModelId) -> Option<ModelPricing> {
        self.static_table.get(provider, model)
    }

    /// Load sample / demo prices for examples (explicit opt-in, not adapter-owned).
    pub fn load_example_prices(&self) {
        let now = Utc::now();
        self.upsert(ModelPricing {
            provider: ProviderId::deepseek(),
            model: ModelId::new("deepseek-chat"),
            input_per_million: Some(Decimal::new(14, 2)),
            output_per_million: Some(Decimal::new(28, 2)),
            cached_input_per_million: Some(Decimal::new(14, 3)),
            effective_from: now,
        });
        self.upsert(ModelPricing {
            provider: ProviderId::openai(),
            model: ModelId::new("gpt-4o-mini"),
            input_per_million: Some(Decimal::new(15, 2)),
            output_per_million: Some(Decimal::new(60, 2)),
            cached_input_per_million: None,
            effective_from: now,
        });
    }
}

#[async_trait]
impl PricingProvider for PricingRegistry {
    async fn get_price(
        &self,
        provider: ProviderId,
        model: ModelId,
    ) -> AiResult<Option<ModelPricing>> {
        Ok(self.get_price_sync(&provider, &model))
    }
}

/// Helper for JSON deserialization errors.
pub fn pricing_parse_error(msg: impl Into<String>) -> AiError {
    AiError::Serialization { message: msg.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_pricing_roundtrip() {
        let p = StaticPricing::new();
        p.upsert(ModelPricing {
            provider: ProviderId::openai(),
            model: ModelId::new("m"),
            input_per_million: Some(Decimal::ONE),
            output_per_million: Some(Decimal::TEN),
            cached_input_per_million: None,
            effective_from: Utc::now(),
        });
        assert!(p.get(&ProviderId::openai(), &ModelId::new("m")).is_some());
    }
}
