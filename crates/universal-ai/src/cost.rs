//! Cost calculation from usage + pricing, and worst-case estimation.
//!
//! Three different amounts exist for every attempt and are never mixed:
//!
//! * **estimate** ([`CostEstimate`]) — a pre-flight upper bound computed from the
//!   request (input byte bound, output token bound, highest applicable rates);
//! * **reservation** — the estimate, held in the spend ledger while the attempt is in
//!   flight (see [`crate::budget`]);
//! * **actual cost** ([`Cost`]) — reported usage × pricing, per token class.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, AiResult};
use crate::pricing::{ModelPricing, PricingRegistry, TierMode, TierRates, TokenClass};
use crate::types::{Currency, ModelId, ProviderId};
use crate::usage::Usage;

/// Actual monetary cost of one attempt, split by token class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    /// Currency.
    pub currency: Currency,
    /// Total amount.
    pub amount: Decimal,
    /// Uncached input portion.
    pub input_cost: Decimal,
    /// Non-reasoning output portion.
    pub output_cost: Decimal,
    /// Cache-read portion.
    pub cache_cost: Decimal,
    /// Cache-write portion.
    #[serde(default)]
    pub cache_write_cost: Decimal,
    /// Reasoning portion.
    #[serde(default)]
    pub reasoning_cost: Decimal,
    /// Threshold of the highest price tier the request reached (`None` = base
    /// rates or a flat price; see [`crate::pricing::Tiering`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_above_input_tokens: Option<u64>,
}

impl Cost {
    /// Zero USD (struct-update base).
    pub fn zero() -> Self {
        Self {
            currency: Currency::usd(),
            amount: Decimal::ZERO,
            input_cost: Decimal::ZERO,
            output_cost: Decimal::ZERO,
            cache_cost: Decimal::ZERO,
            cache_write_cost: Decimal::ZERO,
            reasoning_cost: Decimal::ZERO,
            tier_above_input_tokens: None,
        }
    }
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

/// Where the output token bound of an estimate comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputBoundSource {
    /// No output bound (input-only estimate; not a worst case).
    #[default]
    None,
    /// The request's `max_tokens`.
    RequestMaxTokens,
    /// The model registry's `max_output_tokens` (pinned into the request).
    ModelMaxOutput,
    /// A caller-supplied token count (`CostManager::estimate`).
    Caller,
}

/// How an estimate was obtained — enough to reproduce the number by hand.
/// Contains sizes and rates only, never prompt text or secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimateBreakdown {
    /// UTF-8 bytes of message text (all messages).
    pub input_bytes: u64,
    /// Fixed per-message overhead tokens (role markers, separators).
    pub message_overhead_tokens: u64,
    /// Bytes of tool-call ids / names / arguments carried in messages.
    pub tool_call_bytes: u64,
    /// Bytes of the serialized tool schemas.
    pub tool_schema_bytes: u64,
    /// Fixed overhead for provider-injected tool instructions.
    pub tool_overhead_tokens: u64,
    /// Upper bound on input tokens (sum of the above; 1 token ≤ 1 byte).
    pub input_tokens_upper_bound: u64,
    /// Upper bound on input tokens served from cache (every input token may be).
    pub cached_tokens_upper_bound: u64,
    /// Upper bound on tool-related input tokens (schemas + overhead + call bytes).
    pub tool_tokens_upper_bound: u64,
    /// Upper bound on output tokens (including reasoning).
    pub output_tokens_upper_bound: u64,
    /// Upper bound on reasoning tokens (they count inside the output bound).
    pub reasoning_tokens_upper_bound: u64,
    /// Where the output bound comes from.
    pub output_bound_source: OutputBoundSource,
    /// Rate applied to the input bound: max over priced input-side classes.
    pub input_rate_per_million: Decimal,
    /// Rate applied to the output bound: max over priced output-side classes.
    pub output_rate_per_million: Decimal,
    /// Conditions under which the bound holds (and known gaps).
    pub assumptions: Vec<String>,
    /// Price sheet the estimate was computed with ([`ModelPricing::version`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_version: Option<String>,
}

/// Estimated cost before the call completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Input cost bound.
    pub input: Decimal,
    /// Output cost bound (zero when no output bound was given).
    pub output: Decimal,
    /// Total estimate.
    pub total: Decimal,
    /// Currency.
    pub currency: Currency,
    /// Whether `output` bounds the output (false = input-only estimate).
    pub output_is_upper_bound: bool,
    /// How the numbers were obtained.
    #[serde(default)]
    pub breakdown: EstimateBreakdown,
}

impl CostEstimate {
    /// Human-readable derivation of the estimate (no prompt content).
    pub fn explain(&self) -> String {
        let b = &self.breakdown;
        let mut out = String::new();
        out.push_str(&format!("input bytes: {}\n", b.input_bytes));
        out.push_str(&format!(
            "message overhead: {} tokens\n",
            b.message_overhead_tokens
        ));
        if b.tool_schema_bytes + b.tool_overhead_tokens + b.tool_call_bytes > 0 {
            out.push_str(&format!(
                "tool schema bytes: {} (+{} overhead tokens), tool call bytes: {}\n",
                b.tool_schema_bytes, b.tool_overhead_tokens, b.tool_call_bytes
            ));
        }
        out.push_str(&format!("input tokens <= {}\n", b.input_tokens_upper_bound));
        out.push_str(&format!(
            "max output: {} ({:?}, reasoning included)\n",
            b.output_tokens_upper_bound, b.output_bound_source
        ));
        out.push_str(&format!(
            "input rate: {} {}/1M\n",
            b.input_rate_per_million, self.currency
        ));
        out.push_str(&format!(
            "output rate: {} {}/1M\n",
            b.output_rate_per_million, self.currency
        ));
        out.push_str(&format!(
            "{}: {} {}",
            if self.output_is_upper_bound {
                "worst-case"
            } else {
                "input-only estimate"
            },
            self.total,
            self.currency
        ));
        for a in &b.assumptions {
            out.push_str(&format!("\nassumes: {a}"));
        }
        out
    }
}

impl std::fmt::Display for CostEstimate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Estimated cost:")?;
        writeln!(f, "Input:   {} {}", self.input, self.currency)?;
        writeln!(
            f,
            "Output:  {} {}{}",
            self.output,
            self.currency,
            if self.output_is_upper_bound {
                " (upper bound)"
            } else {
                ""
            }
        )?;
        write!(f, "Total:   {} {}", self.total, self.currency)
    }
}

/// Why a cost could not be determined (unknown cost is never zero).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingGap {
    /// No price sheet for the model.
    NoPrice,
    /// Tokens were used in a class whose policy yields no rate.
    MissingRate(TokenClass),
    /// Provider reported categories that cannot be priced.
    UnpricedCategories(Vec<String>),
    /// Usage counts are inconsistent.
    InconsistentUsage(String),
}

impl std::fmt::Display for PricingGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPrice => write!(f, "no price for model"),
            Self::MissingRate(c) => write!(f, "no rate for used token class {c:?}"),
            Self::UnpricedCategories(c) => {
                write!(f, "unpriced usage categories: {}", c.join(", "))
            }
            Self::InconsistentUsage(m) => write!(f, "{m}"),
        }
    }
}

/// Input-side token bound of a request (see [`crate::budget`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputBound {
    /// Message text bytes.
    pub message_bytes: u64,
    /// Per-message overhead tokens.
    pub message_overhead_tokens: u64,
    /// Tool call id/name/argument bytes inside messages.
    pub tool_call_bytes: u64,
    /// Serialized tool schema bytes.
    pub tool_schema_bytes: u64,
    /// Tool instruction overhead tokens.
    pub tool_overhead_tokens: u64,
}

impl InputBound {
    /// Bound for a plain token count.
    pub fn tokens(n: u64) -> Self {
        Self {
            message_bytes: n,
            ..Default::default()
        }
    }

    /// Total input token bound.
    pub fn total(&self) -> u64 {
        self.message_bytes
            .saturating_add(self.message_overhead_tokens)
            .saturating_add(self.tool_call_bytes)
            .saturating_add(self.tool_schema_bytes)
            .saturating_add(self.tool_overhead_tokens)
    }
}

/// Calculates costs from pricing registry + usage.
#[derive(Debug)]
pub struct CostManager {
    pricing: PricingRegistry,
}

impl CostManager {
    /// Create with a pricing registry.
    pub fn new(pricing: PricingRegistry) -> Self {
        Self { pricing }
    }

    /// Access pricing registry.
    pub fn pricing(&self) -> &PricingRegistry {
        &self.pricing
    }

    /// Mutable pricing registry.
    pub fn pricing_mut(&mut self) -> &mut PricingRegistry {
        &mut self.pricing
    }

    /// Calculate actual cost from usage. `None` when the cost is unknown (see
    /// [`CostManager::price`] for the reason) — never a silent zero.
    pub fn calculate(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        usage: &Usage,
    ) -> AiResult<Option<Cost>> {
        Ok(self.price(provider, model, usage).ok())
    }

    /// Actual cost from usage, or why it cannot be determined.
    pub fn price(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        usage: &Usage,
    ) -> Result<Cost, PricingGap> {
        let price = self
            .pricing
            .get_price_sync(provider, model)
            .ok_or(PricingGap::NoPrice)?;
        cost_from_pricing(&price, usage)
    }

    /// Estimate from plain token counts (input bound, optional output bound).
    /// `Ok(None)` when the model has no price or a needed rate is missing.
    pub fn estimate(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        estimated_input_tokens: u64,
        estimated_output_tokens: Option<u64>,
    ) -> AiResult<Option<CostEstimate>> {
        Ok(self.estimate_bounds(
            provider,
            model,
            InputBound::tokens(estimated_input_tokens),
            estimated_output_tokens.map(|n| (n, OutputBoundSource::Caller)),
        ))
    }

    /// Upper-bound estimate: the input bound at the highest priced input-side rate
    /// plus the output bound at the highest priced output-side rate. `None` when
    /// the model has no price or the input / output rate is missing.
    pub(crate) fn estimate_bounds(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        input: InputBound,
        output: Option<(u64, OutputBoundSource)>,
    ) -> Option<CostEstimate> {
        let price = self.pricing.get_price_sync(provider, model)?;
        estimate_from_pricing(&price, input, output)
    }

    /// Enforce budget against an estimate.
    pub fn check_budget(
        &self,
        estimate: &CostEstimate,
        max_request: Option<Decimal>,
        spent_today: Decimal,
        max_daily: Option<Decimal>,
        spent_month: Decimal,
        max_monthly: Option<Decimal>,
    ) -> AiResult<()> {
        if let Some(max) = max_request {
            if estimate.total > max {
                return Err(AiError::BudgetExceeded {
                    message: format!(
                        "estimated request cost {} exceeds max {}",
                        estimate.total, max
                    ),
                });
            }
        }
        if let Some(max) = max_daily {
            if spent_today + estimate.total > max {
                return Err(AiError::BudgetExceeded {
                    message: format!(
                        "daily budget would exceed: spent={spent_today} estimate={} max={max}",
                        estimate.total
                    ),
                });
            }
        }
        if let Some(max) = max_monthly {
            if spent_month + estimate.total > max {
                return Err(AiError::BudgetExceeded {
                    message: format!(
                        "monthly budget would exceed: spent={spent_month} estimate={} max={max}",
                        estimate.total
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Actual cost per token class. Any used class without a rate, any unpriced
/// provider category and any inconsistent count makes the cost unknown.
pub(crate) fn cost_from_pricing(price: &ModelPricing, usage: &Usage) -> Result<Cost, PricingGap> {
    let unpriced = usage.unpriced_categories();
    if !unpriced.is_empty() {
        return Err(PricingGap::UnpricedCategories(
            unpriced.into_iter().map(str::to_string).collect(),
        ));
    }
    let b = usage.breakdown().map_err(PricingGap::InconsistentUsage)?;
    // The tier the whole prompt reaches (prompt_tokens includes cache reads and
    // writes); output-side classes are always billed at its rates.
    let tier = price.tier_for(usage.prompt_tokens);
    let (input_cost, cache_cost, cache_write_cost) = match price.tier_mode() {
        Some(TierMode::Marginal) => marginal_input_cost(price, &b)?,
        Some(TierMode::WholeRequest) | None => (
            charge(&tier, TokenClass::Input, b.input)?,
            charge(&tier, TokenClass::CachedInput, b.cached_input)?,
            charge(&tier, TokenClass::CacheCreation, b.cache_creation)?,
        ),
    };
    let output_cost = charge(&tier, TokenClass::Output, b.output)?;
    let reasoning_cost = charge(&tier, TokenClass::Reasoning, b.reasoning)?;
    Ok(Cost {
        currency: Currency::usd(),
        amount: input_cost + cache_cost + cache_write_cost + output_cost + reasoning_cost,
        input_cost,
        output_cost,
        cache_cost,
        cache_write_cost,
        reasoning_cost,
        tier_above_input_tokens: tier.tier.map(|_| tier.above_input_tokens),
    })
}

/// `tokens` of `class` at `rates`; a used class without a rate is unknown.
fn charge(rates: &TierRates, class: TokenClass, tokens: u64) -> Result<Decimal, PricingGap> {
    if tokens == 0 {
        return Ok(Decimal::ZERO);
    }
    let (rate, _) = rates.rate(class).ok_or(PricingGap::MissingRate(class))?;
    Ok(per_million(rate, tokens))
}

/// [`TierMode::Marginal`] input cost: prompt positions `1..=T1` at the base
/// rates, `T1+1..=T2` at tier 0, … — cache reads first, then cache writes, then
/// uncached input. Returns (input, cache read, cache write) cost.
fn marginal_input_cost(
    price: &ModelPricing,
    b: &crate::usage::TokenBreakdown,
) -> Result<(Decimal, Decimal, Decimal), PricingGap> {
    let tiers = price.all_tiers();
    let mut placed = 0u64;
    let mut band_cost = |class: TokenClass, mut tokens: u64| -> Result<Decimal, PricingGap> {
        let mut total = Decimal::ZERO;
        while tokens > 0 {
            // The next token sits at position `placed + 1`; a tier covers the
            // positions strictly above its threshold.
            let i = tiers
                .iter()
                .rposition(|t| t.tier.is_none() || placed >= t.above_input_tokens)
                .unwrap_or(0);
            let band_end = tiers
                .get(i + 1)
                .map_or(u64::MAX, |next| next.above_input_tokens);
            let n = tokens.min(band_end - placed);
            total += charge(&tiers[i], class, n)?;
            placed += n;
            tokens -= n;
        }
        Ok(total)
    };
    let cache = band_cost(TokenClass::CachedInput, b.cached_input)?;
    let cache_write = band_cost(TokenClass::CacheCreation, b.cache_creation)?;
    let input = band_cost(TokenClass::Input, b.input)?;
    Ok((input, cache, cache_write))
}

/// Upper bound for `price`: the input bound at the highest input-side rate and the
/// output bound at the highest output-side rate of every tier the input bound
/// can reach. This dominates both tier modes: a request can only be billed at
/// the rates of tiers its prompt reaches, and never above the highest of them.
pub(crate) fn estimate_from_pricing(
    price: &ModelPricing,
    input: InputBound,
    output: Option<(u64, OutputBoundSource)>,
) -> Option<CostEstimate> {
    let input_tokens = input.total();
    let reachable: Vec<TierRates> = price
        .all_tiers()
        .into_iter()
        .filter(|t| t.tier.is_none() || input_tokens > t.above_input_tokens)
        .collect();
    let max_rate = |classes: &[TokenClass]| -> Option<Decimal> {
        reachable
            .iter()
            .flat_map(|t| classes.iter().filter_map(|c| t.rate(*c).map(|(r, _)| r)))
            .max()
    };
    // The plain input / output rates must exist; other classes only raise the bound.
    price.rate(TokenClass::Input)?;
    let input_rate = max_rate(&[
        TokenClass::Input,
        TokenClass::CachedInput,
        TokenClass::CacheCreation,
    ])?;
    let output_rate = match output {
        Some(_) => {
            price.rate(TokenClass::Output)?;
            max_rate(&[TokenClass::Output, TokenClass::Reasoning])?
        }
        None => Decimal::ZERO,
    };
    let (output_tokens, source) = output.unwrap_or((0, OutputBoundSource::None));
    let input_cost = per_million(input_rate, input_tokens);
    let output_cost = per_million(output_rate, output_tokens);

    let mut assumptions = vec![
        "1 input token <= 1 UTF-8 byte (byte-level tokenizers)".to_string(),
        "reasoning tokens count toward the output bound (max_tokens)".to_string(),
    ];
    if price.rate(TokenClass::CacheCreation).is_none() {
        assumptions.push(
            "no cache-write rate: universal-ai never requests prompt-cache writes; a \
             response reporting cache-creation tokens settles as PricingUnavailable"
                .to_string(),
        );
    }
    if source == OutputBoundSource::None {
        assumptions.push("no output bound: this is not a worst case".to_string());
    }
    if let Some(tiering) = &price.tiering {
        let mode = price.tier_mode().unwrap_or(TierMode::WholeRequest);
        match reachable.last().filter(|t| t.tier.is_some()) {
            Some(top) => assumptions.push(format!(
                "tiered price ({mode:?}): input bound {input_tokens} exceeds {}; the \
                 highest rates of the base and {} reachable tier(s) apply",
                top.above_input_tokens,
                reachable.len() - 1
            )),
            None => assumptions.push(format!(
                "tiered price ({mode:?}): input bound {input_tokens} <= first tier \
                 threshold {}; base rates apply",
                tiering.tiers.first().map_or(0, |t| t.above_input_tokens)
            )),
        }
    }
    let tool_tokens = input
        .tool_schema_bytes
        .saturating_add(input.tool_overhead_tokens)
        .saturating_add(input.tool_call_bytes);
    Some(CostEstimate {
        input: input_cost,
        output: output_cost,
        total: input_cost + output_cost,
        currency: Currency::usd(),
        output_is_upper_bound: output.is_some(),
        breakdown: EstimateBreakdown {
            input_bytes: input.message_bytes,
            message_overhead_tokens: input.message_overhead_tokens,
            tool_call_bytes: input.tool_call_bytes,
            tool_schema_bytes: input.tool_schema_bytes,
            tool_overhead_tokens: input.tool_overhead_tokens,
            input_tokens_upper_bound: input_tokens,
            cached_tokens_upper_bound: input_tokens,
            tool_tokens_upper_bound: tool_tokens,
            output_tokens_upper_bound: output_tokens,
            reasoning_tokens_upper_bound: output_tokens,
            output_bound_source: source,
            input_rate_per_million: input_rate,
            output_rate_per_million: output_rate,
            assumptions,
            pricing_version: Some(price.version()),
        },
    })
}

fn per_million(rate: Decimal, tokens: u64) -> Decimal {
    if tokens == 0 {
        return Decimal::ZERO;
    }
    rate * Decimal::from(tokens) / Decimal::from(1_000_000u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: i64, output: i64) -> ModelPricing {
        ModelPricing::per_million(
            ProviderId::deepseek(),
            "m",
            Decimal::from(input),
            Decimal::from(output),
        )
    }

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Default::default()
        }
    }

    #[test]
    fn calculates_from_usage() {
        let mut p = price(0, 0);
        p.input_per_million = Some(Decimal::new(14, 2));
        p.output_per_million = Some(Decimal::new(28, 2));
        p.cached_input_per_million = Some(Decimal::new(14, 3));
        let cost = cost_from_pricing(&p, &usage(1_000_000, 500_000)).unwrap();
        assert_eq!(cost.input_cost, Decimal::new(14, 2));
        assert_eq!(cost.output_cost, Decimal::new(14, 2)); // 0.28 * 0.5
    }

    #[test]
    fn missing_rate_is_unknown_not_zero() {
        let mut p = price(1, 0);
        p.output_per_million = None;
        let u = Usage {
            cached_tokens: Some(400_000),
            ..usage(1_000_000, 10)
        };
        assert_eq!(
            cost_from_pricing(&p, &u),
            Err(PricingGap::MissingRate(TokenClass::Output))
        );

        // Cached tokens without a cached rate are billed at the input rate.
        let u = Usage {
            completion_tokens: 0,
            ..u
        };
        let cost = cost_from_pricing(&p, &u).unwrap();
        assert_eq!(cost.amount, Decimal::ONE);
    }

    #[test]
    fn cache_creation_without_rate_is_unknown() {
        let p = price(1, 2);
        let u = Usage {
            cache_creation_tokens: Some(10),
            ..usage(100, 1)
        };
        assert_eq!(
            cost_from_pricing(&p, &u),
            Err(PricingGap::MissingRate(TokenClass::CacheCreation))
        );
    }

    #[test]
    fn unknown_categories_and_inconsistent_usage_are_unknown() {
        let p = price(1, 2);
        let mut u = usage(100, 1);
        u.other_tokens.insert("input_audio".into(), 5);
        assert!(matches!(
            cost_from_pricing(&p, &u),
            Err(PricingGap::UnpricedCategories(_))
        ));
        u.other_tokens.insert("input_audio".into(), 0);
        assert!(
            cost_from_pricing(&p, &u).is_ok(),
            "zero counts are harmless"
        );
        let bad = Usage {
            reasoning_tokens: Some(5),
            ..usage(1, 1)
        };
        assert!(matches!(
            cost_from_pricing(&p, &bad),
            Err(PricingGap::InconsistentUsage(_))
        ));
    }

    #[test]
    fn reasoning_billed_at_its_own_or_output_rate() {
        let mut p = price(0, 10);
        let u = Usage {
            reasoning_tokens: Some(1_000_000),
            ..usage(0, 2_000_000)
        };
        let c = cost_from_pricing(&p, &u).unwrap();
        assert_eq!(
            (c.output_cost, c.reasoning_cost),
            (Decimal::TEN, Decimal::TEN)
        );
        p.reasoning_per_million = Some(Decimal::from(30));
        let c = cost_from_pricing(&p, &u).unwrap();
        assert_eq!(c.reasoning_cost, Decimal::from(30));
    }

    #[test]
    fn estimate_uses_highest_applicable_rates() {
        let mut p = price(1, 2);
        p.cache_write_per_million = Some(Decimal::from(3));
        p.reasoning_per_million = Some(Decimal::from(5));
        let e = estimate_from_pricing(
            &p,
            InputBound::tokens(1_000_000),
            Some((1_000_000, OutputBoundSource::RequestMaxTokens)),
        )
        .unwrap();
        assert_eq!(e.input, Decimal::from(3));
        assert_eq!(e.output, Decimal::from(5));
        assert!(e.explain().contains("worst-case: 8"));

        // The estimate dominates any usage consistent with the bounds.
        let worst_usage = Usage {
            cache_creation_tokens: Some(1_000_000),
            reasoning_tokens: Some(1_000_000),
            ..usage(1_000_000, 1_000_000)
        };
        assert!(cost_from_pricing(&p, &worst_usage).unwrap().amount <= e.total);
    }

    /// Base 1 / 10 (cached 0.5), above 100 input tokens 2 / 20 (cached 1),
    /// above 1000 input tokens 4 / 40 (cached 2) — USD per 1M.
    fn tiered(mode: TierMode) -> ModelPricing {
        use crate::pricing::{PriceTier, Tiering};
        let tier = |above: u64, input: i64, output: i64, cached: i64| PriceTier {
            above_input_tokens: above,
            input_per_million: Decimal::from(input),
            output_per_million: Decimal::from(output),
            cached_input_per_million: Some(Decimal::from(cached)),
            cache_write_per_million: None,
            reasoning_per_million: None,
        };
        let mut p = price(1, 10);
        p.cached_input_per_million = Some(Decimal::new(5, 1));
        p.tiering = Some(Tiering {
            mode,
            tiers: vec![tier(100, 2, 20, 1), tier(1_000, 4, 40, 2)],
        });
        p
    }

    /// `n` USD per 1M tokens × `tokens`.
    fn usd(n: i64, tokens: u64) -> Decimal {
        per_million(Decimal::from(n), tokens)
    }

    #[test]
    fn whole_request_bills_every_class_at_the_reached_tier() {
        let p = tiered(TierMode::WholeRequest);
        // Below and exactly at the threshold: base rates.
        for prompt in [99, 100] {
            let c = cost_from_pricing(&p, &usage(prompt, 7)).unwrap();
            assert_eq!(c.input_cost, usd(1, prompt));
            assert_eq!(c.output_cost, usd(10, 7));
            assert_eq!(c.tier_above_input_tokens, None);
        }
        // One token above: the whole request moves, output and cache included.
        let u = Usage {
            cached_tokens: Some(60),
            ..usage(101, 7)
        };
        let c = cost_from_pricing(&p, &u).unwrap();
        assert_eq!(c.cache_cost, usd(1, 60));
        assert_eq!(c.input_cost, usd(2, 41));
        assert_eq!(c.output_cost, usd(20, 7));
        assert_eq!(c.tier_above_input_tokens, Some(100));
        let c = cost_from_pricing(&p, &usage(1_001, 1)).unwrap();
        assert_eq!(c.amount, usd(4, 1_001) + usd(40, 1));
        assert_eq!(c.tier_above_input_tokens, Some(1_000));
    }

    #[test]
    fn marginal_bills_each_band_at_its_own_rate() {
        let p = tiered(TierMode::Marginal);
        // At the threshold nothing is in the upper band.
        let c = cost_from_pricing(&p, &usage(100, 7)).unwrap();
        assert_eq!(c.amount, usd(1, 100) + usd(10, 7));
        // 150 input: 100 at the base rate, 50 above; output at the reached tier.
        let c = cost_from_pricing(&p, &usage(150, 7)).unwrap();
        assert_eq!(c.input_cost, usd(1, 100) + usd(2, 50));
        assert_eq!(c.output_cost, usd(20, 7));
        assert_eq!(c.tier_above_input_tokens, Some(100));
        // Cache reads fill the prompt first: 120 cached = 100 base + 20 tier 0,
        // then 30 uncached in tier 0.
        let u = Usage {
            cached_tokens: Some(120),
            ..usage(150, 0)
        };
        let c = cost_from_pricing(&p, &u).unwrap();
        assert_eq!(c.cache_cost, usd_frac(5, 1, 100) + usd(1, 20));
        assert_eq!(c.input_cost, usd(2, 30));
        // Three bands.
        let c = cost_from_pricing(&p, &usage(1_500, 0)).unwrap();
        assert_eq!(c.input_cost, usd(1, 100) + usd(2, 900) + usd(4, 500));
        assert_eq!(c.tier_above_input_tokens, Some(1_000));
    }

    /// `mantissa`e-`scale` USD per 1M × `tokens`.
    fn usd_frac(mantissa: i64, scale: u32, tokens: u64) -> Decimal {
        per_million(Decimal::new(mantissa, scale), tokens)
    }

    #[test]
    fn tiered_estimate_uses_every_reachable_tier() {
        for mode in [TierMode::WholeRequest, TierMode::Marginal] {
            let p = tiered(mode);
            let out = Some((10, OutputBoundSource::RequestMaxTokens));
            // An input bound at the threshold cannot reach the tier.
            let e = estimate_from_pricing(&p, InputBound::tokens(100), out).unwrap();
            assert_eq!(e.breakdown.input_rate_per_million, Decimal::ONE);
            assert_eq!(e.breakdown.output_rate_per_million, Decimal::TEN);
            assert!(e.explain().contains("base rates apply"), "{}", e.explain());
            // One more token can: the expensive tier is reserved.
            let e = estimate_from_pricing(&p, InputBound::tokens(101), out).unwrap();
            assert_eq!(e.breakdown.input_rate_per_million, Decimal::TWO);
            assert_eq!(e.breakdown.output_rate_per_million, Decimal::from(20));
            assert_eq!(e.total, usd(2, 101) + usd(20, 10));
            assert!(e.explain().contains("exceeds 100"), "{}", e.explain());
            assert_eq!(e.breakdown.pricing_version, Some(p.version()));
            let e = estimate_from_pricing(&p, InputBound::tokens(5_000), out).unwrap();
            assert_eq!(e.breakdown.input_rate_per_million, Decimal::from(4));
        }
        // A cheaper upper tier never lowers the bound below a reachable rate.
        let mut p = tiered(TierMode::WholeRequest);
        p.tiering.as_mut().unwrap().tiers[1].output_per_million = Decimal::ONE;
        let e = estimate_from_pricing(
            &p,
            InputBound::tokens(5_000),
            Some((10, OutputBoundSource::RequestMaxTokens)),
        )
        .unwrap();
        assert_eq!(e.breakdown.output_rate_per_million, Decimal::from(20));
    }

    #[test]
    fn budget_blocks_overspend() {
        let mgr = CostManager::new(PricingRegistry::new());
        let estimate = CostEstimate {
            input: Decimal::new(1, 2),
            output: Decimal::new(1, 2),
            total: Decimal::new(2, 2),
            currency: Currency::usd(),
            output_is_upper_bound: true,
            breakdown: EstimateBreakdown::default(),
        };
        let err = mgr
            .check_budget(
                &estimate,
                Some(Decimal::new(1, 2)),
                Decimal::ZERO,
                None,
                Decimal::ZERO,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, AiError::BudgetExceeded { .. }));
    }
}
