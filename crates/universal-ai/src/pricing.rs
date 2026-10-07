//! Pricing registry — prices live outside provider adapters.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::{AiError, AiResult};
use crate::types::{ModelId, ProviderId};

/// Price sheet entry for a model (USD per 1M tokens per token class).
///
/// Every [`TokenClass`] has an explicit policy (see [`ModelPricing::rate`]); a
/// class whose policy yields no rate makes the cost of any usage in that class
/// unknown — it is never priced at another class's rate when that could
/// underestimate.
///
/// The rates below are the **base tier** (input tokens up to the first tier's
/// threshold). Models whose price depends on the prompt size add explicit
/// [`Tiering`]; nothing is assumed about a sheet without it except that its
/// price is flat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Provider.
    pub provider: ProviderId,
    /// Model.
    pub model: ModelId,
    /// USD per 1M uncached input tokens.
    pub input_per_million: Option<Decimal>,
    /// USD per 1M non-reasoning output tokens.
    pub output_per_million: Option<Decimal>,
    /// USD per 1M input tokens read from the prompt cache.
    pub cached_input_per_million: Option<Decimal>,
    /// USD per 1M input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<Decimal>,
    /// USD per 1M reasoning tokens (when billed differently from output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million: Option<Decimal>,
    /// When this price becomes effective.
    pub effective_from: DateTime<Utc>,
    /// Prompt-size tiers above the base rates (`None` = flat price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiering: Option<Tiering>,
}

/// How tier rates apply once the prompt crosses a threshold.
///
/// The tier measure is the request's **input tokens** — `Usage::prompt_tokens`,
/// which every adapter normalizes to include cache reads and cache writes. A tier
/// with threshold `T` applies when input tokens are **strictly greater** than
/// `T`: with `T = 200_000`, a 200 000-token prompt is still in the lower tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierMode {
    /// The whole request is billed at the rates of the tier its input size falls
    /// into: every token class (input, cache, output, reasoning) of a 250k prompt
    /// uses the `> 200k` rates.
    WholeRequest,
    /// Input tokens are billed per band: the first `T1` at the base rates, the
    /// next `T2 − T1` at tier 1, and so on. Within the prompt, cache reads come
    /// first, then cache writes, then uncached input (the prefix-cache layout).
    /// Output and reasoning tokens are billed at the rates of the tier the whole
    /// prompt reaches.
    Marginal,
}

/// Explicit prompt-size tiers of a [`ModelPricing`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tiering {
    /// How tier rates apply (required: there is no default mode).
    pub mode: TierMode,
    /// Tiers above the base rates, thresholds strictly increasing.
    pub tiers: Vec<PriceTier>,
}

/// Rates that apply above an input-token threshold (USD per 1M tokens).
///
/// A tier states its input and output rates and exactly the optional rates
/// (cached input, cache write, reasoning) the base states — nothing is inherited
/// from a lower tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTier {
    /// The tier applies when input tokens are strictly greater than this.
    pub above_input_tokens: u64,
    /// USD per 1M uncached input tokens.
    pub input_per_million: Decimal,
    /// USD per 1M non-reasoning output tokens.
    pub output_per_million: Decimal,
    /// USD per 1M input tokens read from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_million: Option<Decimal>,
    /// USD per 1M input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<Decimal>,
    /// USD per 1M reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million: Option<Decimal>,
}

/// The rates of one tier (or of the base), with the per-class policy of
/// [`ModelPricing::rate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierRates {
    /// Threshold the tier applies above (`0` for the base rates).
    pub above_input_tokens: u64,
    /// Index into [`Tiering::tiers`]; `None` for the base rates.
    pub tier: Option<usize>,
    input: Option<Decimal>,
    output: Option<Decimal>,
    cached_input: Option<Decimal>,
    cache_write: Option<Decimal>,
    reasoning: Option<Decimal>,
}

impl TierRates {
    /// Rate (USD per 1M tokens) for `class` in this tier, or `None` when the class
    /// cannot be priced (policy: [`ModelPricing::rate`]).
    pub fn rate(&self, class: TokenClass) -> Option<(Decimal, RateSource)> {
        let explicit = |r: Option<Decimal>| r.map(|r| (r, RateSource::Explicit));
        match class {
            TokenClass::Input => explicit(self.input),
            TokenClass::CachedInput => explicit(self.cached_input)
                .or_else(|| self.input.map(|r| (r, RateSource::InputRateConservative))),
            TokenClass::CacheCreation => explicit(self.cache_write),
            TokenClass::Output => explicit(self.output),
            TokenClass::Reasoning => explicit(self.reasoning)
                .or_else(|| self.output.map(|r| (r, RateSource::OutputRate))),
        }
    }
}

/// Disjoint token billing classes (see [`crate::usage::TokenBreakdown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenClass {
    /// Uncached input.
    Input,
    /// Input read from the prompt cache.
    CachedInput,
    /// Input written to the prompt cache.
    CacheCreation,
    /// Non-reasoning output.
    Output,
    /// Reasoning / thinking output.
    Reasoning,
}

impl TokenClass {
    /// All classes.
    pub const ALL: [TokenClass; 5] = [
        TokenClass::Input,
        TokenClass::CachedInput,
        TokenClass::CacheCreation,
        TokenClass::Output,
        TokenClass::Reasoning,
    ];
}

/// Where the rate used for a [`TokenClass`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    /// The class's own price field.
    Explicit,
    /// Cache reads without a cached rate: billed at the full input rate. Every
    /// supported provider discounts cache reads, so this can only overestimate.
    InputRateConservative,
    /// Reasoning without a reasoning rate: billed at the output rate, which is how
    /// OpenAI, Anthropic and Gemini bill reasoning tokens.
    OutputRate,
}

impl ModelPricing {
    /// Price with input / output rates only (other classes follow their policy).
    pub fn per_million(
        provider: ProviderId,
        model: impl Into<ModelId>,
        input: Decimal,
        output: Decimal,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            input_per_million: Some(input),
            output_per_million: Some(output),
            cached_input_per_million: None,
            cache_write_per_million: None,
            reasoning_per_million: None,
            effective_from: Utc::now(),
            tiering: None,
        }
    }

    /// Rate (USD per 1M tokens) for `class` at the **base** rates, or `None` when
    /// the class cannot be priced. Policy (the same in every tier):
    ///
    /// | class            | rate                                   |
    /// |------------------|----------------------------------------|
    /// | `Input`          | `input_per_million`                    |
    /// | `CachedInput`    | `cached_input_per_million`, else input (overestimate) |
    /// | `CacheCreation`  | `cache_write_per_million` only (writes cost more than input) |
    /// | `Output`         | `output_per_million`                   |
    /// | `Reasoning`      | `reasoning_per_million`, else output   |
    pub fn rate(&self, class: TokenClass) -> Option<(Decimal, RateSource)> {
        self.base_rates().rate(class)
    }

    /// The base rates (input tokens up to the first tier's threshold).
    pub fn base_rates(&self) -> TierRates {
        TierRates {
            above_input_tokens: 0,
            tier: None,
            input: self.input_per_million,
            output: self.output_per_million,
            cached_input: self.cached_input_per_million,
            cache_write: self.cache_write_per_million,
            reasoning: self.reasoning_per_million,
        }
    }

    /// The base rates followed by every tier, thresholds increasing.
    pub fn all_tiers(&self) -> Vec<TierRates> {
        let mut out = vec![self.base_rates()];
        if let Some(t) = &self.tiering {
            out.extend(t.tiers.iter().enumerate().map(|(i, t)| TierRates {
                above_input_tokens: t.above_input_tokens,
                tier: Some(i),
                input: Some(t.input_per_million),
                output: Some(t.output_per_million),
                cached_input: t.cached_input_per_million,
                cache_write: t.cache_write_per_million,
                reasoning: t.reasoning_per_million,
            }));
        }
        out
    }

    /// Rates of the tier a request with `input_tokens` input tokens falls into:
    /// the last tier whose threshold is strictly below `input_tokens`, else the
    /// base.
    pub fn tier_for(&self, input_tokens: u64) -> TierRates {
        self.all_tiers()
            .into_iter()
            .rev()
            .find(|t| t.tier.is_none() || input_tokens > t.above_input_tokens)
            .unwrap_or_else(|| self.base_rates())
    }

    /// How tiers apply (`None` for a flat price).
    pub fn tier_mode(&self) -> Option<TierMode> {
        self.tiering.as_ref().map(|t| t.mode)
    }

    /// Content address of this price sheet: identical sheets share a version and
    /// any change (a rate, a tier, `effective_from`) gives a new one. Cost records
    /// keep the version they were priced with, so a later price change never
    /// rewrites history.
    pub fn version(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let hex: String = digest.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("pv1-{hex}")
    }

    /// Reject negative rates (they would turn usage into credit) and ambiguous
    /// tiers: thresholds must be positive and strictly increasing, a tiered sheet
    /// needs base input and output rates, and every tier must state exactly the
    /// optional rates the base states.
    pub fn validate(&self) -> AiResult<()> {
        let invalid = |message: String| {
            Err(AiError::Config {
                message: format!("{message} for {}/{}", self.provider, self.model),
            })
        };
        for tier in self.all_tiers() {
            for class in TokenClass::ALL {
                if let Some((r, _)) = tier.rate(class) {
                    if r.is_sign_negative() {
                        return invalid(match tier.tier {
                            None => format!("negative {class:?} rate"),
                            Some(i) => format!("negative {class:?} rate in tier {i}"),
                        });
                    }
                }
            }
        }
        let Some(tiering) = &self.tiering else {
            return Ok(());
        };
        if tiering.tiers.is_empty() {
            return invalid("tiering without tiers".into());
        }
        if self.input_per_million.is_none() || self.output_per_million.is_none() {
            return invalid("a tiered price needs base input and output rates".into());
        }
        let mut last = 0u64;
        for (i, t) in tiering.tiers.iter().enumerate() {
            if t.above_input_tokens <= last {
                return invalid(format!(
                    "tier {i}: threshold {} must be above {last}",
                    t.above_input_tokens
                ));
            }
            last = t.above_input_tokens;
            for (name, base, tier) in [
                (
                    "cached_input_per_million",
                    self.cached_input_per_million,
                    t.cached_input_per_million,
                ),
                (
                    "cache_write_per_million",
                    self.cache_write_per_million,
                    t.cache_write_per_million,
                ),
                (
                    "reasoning_per_million",
                    self.reasoning_per_million,
                    t.reasoning_per_million,
                ),
            ] {
                if base.is_some() != tier.is_some() {
                    return invalid(format!(
                        "tier {i}: {name} must be set exactly when the base rates set it"
                    ));
                }
            }
        }
        Ok(())
    }
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
///
/// Besides the current price of each model it keeps every price sheet it has
/// held, by [`ModelPricing::version`]: an attempt priced with a sheet that was
/// replaced mid-flight still settles — and is later reconciled — with that sheet.
#[derive(Debug, Default)]
pub struct PricingRegistry {
    static_table: StaticPricing,
    history: RwLock<HashMap<String, ModelPricing>>,
}

impl PricingRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a static price (runtime-updatable; not baked into adapters).
    /// A price sheet with a negative rate is ignored (and logged).
    pub fn upsert(&self, pricing: ModelPricing) {
        if let Err(err) = pricing.validate() {
            tracing::error!(error = %err, "rejected invalid model pricing");
            return;
        }
        self.insert(pricing);
    }

    /// Like [`PricingRegistry::upsert`] but reports invalid price sheets.
    pub fn try_upsert(&self, pricing: ModelPricing) -> AiResult<()> {
        pricing.validate()?;
        self.insert(pricing);
        Ok(())
    }

    fn insert(&self, pricing: ModelPricing) {
        if let Ok(mut h) = self.history.write() {
            h.entry(pricing.version())
                .or_insert_with(|| pricing.clone());
        }
        self.static_table.upsert(pricing);
    }

    /// A price sheet this registry has held, by [`ModelPricing::version`].
    pub fn get_version(&self, version: &str) -> Option<ModelPricing> {
        self.history.read().ok()?.get(version).cloned()
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
            cache_write_per_million: None,
            reasoning_per_million: None,
            effective_from: now,
            tiering: None,
        });
        self.upsert(ModelPricing {
            provider: ProviderId::openai(),
            model: ModelId::new("gpt-4o-mini"),
            input_per_million: Some(Decimal::new(15, 2)),
            output_per_million: Some(Decimal::new(60, 2)),
            cached_input_per_million: None,
            cache_write_per_million: None,
            reasoning_per_million: None,
            effective_from: now,
            tiering: None,
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
    AiError::Serialization {
        message: msg.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_pricing_roundtrip() {
        let p = StaticPricing::new();
        p.upsert(ModelPricing::per_million(
            ProviderId::openai(),
            "m",
            Decimal::ONE,
            Decimal::TEN,
        ));
        assert!(p.get(&ProviderId::openai(), &ModelId::new("m")).is_some());
    }

    #[test]
    fn every_class_has_an_explicit_policy() {
        let mut p =
            ModelPricing::per_million(ProviderId::anthropic(), "m", Decimal::ONE, Decimal::TEN);
        assert_eq!(
            p.rate(TokenClass::Input),
            Some((Decimal::ONE, RateSource::Explicit))
        );
        assert_eq!(
            p.rate(TokenClass::CachedInput),
            Some((Decimal::ONE, RateSource::InputRateConservative))
        );
        assert_eq!(
            p.rate(TokenClass::CacheCreation),
            None,
            "never priced as input"
        );
        assert_eq!(
            p.rate(TokenClass::Reasoning),
            Some((Decimal::TEN, RateSource::OutputRate))
        );
        p.cache_write_per_million = Some(Decimal::TWO);
        assert_eq!(
            p.rate(TokenClass::CacheCreation),
            Some((Decimal::TWO, RateSource::Explicit))
        );
    }

    fn tiered(mode: TierMode) -> ModelPricing {
        let mut p =
            ModelPricing::per_million(ProviderId::gemini(), "m", Decimal::ONE, Decimal::TEN);
        p.tiering = Some(Tiering {
            mode,
            tiers: vec![
                PriceTier {
                    above_input_tokens: 100,
                    input_per_million: Decimal::TWO,
                    output_per_million: Decimal::from(20),
                    cached_input_per_million: None,
                    cache_write_per_million: None,
                    reasoning_per_million: None,
                },
                PriceTier {
                    above_input_tokens: 1_000,
                    input_per_million: Decimal::from(4),
                    output_per_million: Decimal::from(40),
                    cached_input_per_million: None,
                    cache_write_per_million: None,
                    reasoning_per_million: None,
                },
            ],
        });
        p
    }

    #[test]
    fn tier_applies_strictly_above_its_threshold() {
        let p = tiered(TierMode::WholeRequest);
        let input = |n: u64| p.tier_for(n).rate(TokenClass::Input).unwrap().0;
        assert_eq!(input(0), Decimal::ONE);
        assert_eq!(
            input(100),
            Decimal::ONE,
            "exactly the threshold: lower tier"
        );
        assert_eq!(input(101), Decimal::TWO);
        assert_eq!(input(1_000), Decimal::TWO);
        assert_eq!(input(1_001), Decimal::from(4));
        assert_eq!(p.tier_for(1_001).tier, Some(1));
        assert_eq!(p.tier_for(5).tier, None);
        // Per-class policy holds inside a tier.
        assert_eq!(
            p.tier_for(500).rate(TokenClass::Reasoning),
            Some((Decimal::from(20), RateSource::OutputRate))
        );
        assert_eq!(p.tier_for(500).rate(TokenClass::CacheCreation), None);
        // A flat price has only the base.
        let flat = ModelPricing::per_million(ProviderId::openai(), "m", Decimal::ONE, Decimal::TEN);
        assert_eq!(flat.all_tiers().len(), 1);
        assert_eq!(flat.tier_for(u64::MAX), flat.base_rates());
    }

    #[test]
    fn ambiguous_tiers_are_rejected() {
        let check = |f: &dyn Fn(&mut ModelPricing), needle: &str| {
            let mut p = tiered(TierMode::Marginal);
            f(&mut p);
            let err = p.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        };
        assert!(tiered(TierMode::Marginal).validate().is_ok());
        fn tiers(p: &mut ModelPricing) -> &mut Vec<PriceTier> {
            &mut p.tiering.as_mut().unwrap().tiers
        }
        check(
            &|p| tiers(p)[1].above_input_tokens = 100,
            "must be above 100",
        );
        check(&|p| tiers(p)[0].above_input_tokens = 0, "must be above 0");
        check(&|p| tiers(p).clear(), "without tiers");
        check(&|p| p.output_per_million = None, "base input and output");
        check(
            &|p| tiers(p)[1].input_per_million = -Decimal::ONE,
            "negative Input rate in tier 1",
        );
        // Optional rates are never inherited: a tier states exactly the base's.
        check(
            &|p| tiers(p)[0].cached_input_per_million = Some(Decimal::ONE),
            "tier 0: cached_input_per_million",
        );
        check(
            &|p| p.cache_write_per_million = Some(Decimal::ONE),
            "tier 0: cache_write_per_million",
        );
    }

    #[test]
    fn version_is_a_content_address() {
        let p = tiered(TierMode::WholeRequest);
        assert_eq!(p.version(), p.clone().version());
        assert!(p.version().starts_with("pv1-"));
        // Survives storage as JSON.
        let back: ModelPricing = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.version(), p.version());
        // Any change is a new version.
        let mut q = p.clone();
        q.tiering.as_mut().unwrap().tiers[1].output_per_million = Decimal::from(41);
        assert_ne!(q.version(), p.version());
        let mut q = p.clone();
        q.tiering.as_mut().unwrap().mode = TierMode::Marginal;
        assert_ne!(q.version(), p.version());
        // Sheets stored before tiers existed still parse, as flat prices.
        let flat = ModelPricing::per_million(ProviderId::openai(), "m", Decimal::ONE, Decimal::TEN);
        let json = serde_json::to_string(&flat).unwrap();
        assert!(!json.contains("tiering"));
        let back: ModelPricing = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tiering, None);
        assert_eq!(back.version(), flat.version());
    }

    #[test]
    fn registry_keeps_replaced_sheets_by_version() {
        let reg = PricingRegistry::new();
        let old = ModelPricing::per_million(ProviderId::openai(), "m", Decimal::ONE, Decimal::TEN);
        let mut new = old.clone();
        new.input_per_million = Some(Decimal::TWO);
        reg.upsert(old.clone());
        reg.upsert(new.clone());
        let current = reg
            .get_price_sync(&ProviderId::openai(), &ModelId::new("m"))
            .unwrap();
        assert_eq!(current, new);
        assert_eq!(reg.get_version(&old.version()), Some(old));
        assert_eq!(reg.get_version(&new.version()), Some(new));
        assert_eq!(reg.get_version("pv1-unknown"), None);
    }

    #[test]
    fn negative_rates_are_rejected() {
        let reg = PricingRegistry::new();
        let bad = ModelPricing::per_million(ProviderId::openai(), "m", -Decimal::ONE, Decimal::ONE);
        assert!(reg.try_upsert(bad.clone()).is_err());
        reg.upsert(bad);
        assert!(reg
            .get_price_sync(&ProviderId::openai(), &ModelId::new("m"))
            .is_none());
    }
}
