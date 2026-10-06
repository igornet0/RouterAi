//! Cost calculation from usage + pricing.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, AiResult};
use crate::pricing::{ModelPricing, PricingRegistry};
use crate::types::{Currency, ModelId, ProviderId};
use crate::usage::Usage;

/// Actual or estimated monetary cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    /// Currency.
    pub currency: Currency,
    /// Total amount.
    pub amount: Decimal,
    /// Input portion.
    pub input_cost: Decimal,
    /// Output portion.
    pub output_cost: Decimal,
    /// Cache portion.
    pub cache_cost: Decimal,
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

/// Estimated cost before the call completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Estimated input cost.
    pub input: Decimal,
    /// Estimated output cost (uses max_tokens when provided).
    pub output: Decimal,
    /// Total estimate.
    pub total: Decimal,
    /// Currency.
    pub currency: Currency,
    /// Whether output is a bound (max) rather than precise.
    pub output_is_upper_bound: bool,
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

    /// Calculate actual cost from usage.
    pub fn calculate(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        usage: &Usage,
    ) -> AiResult<Option<Cost>> {
        let Some(price) = self.pricing.get_price_sync(provider, model) else {
            return Ok(None);
        };
        Ok(Some(cost_from_pricing(&price, usage)))
    }

    /// Estimate cost before execution.
    pub fn estimate(
        &self,
        provider: &ProviderId,
        model: &ModelId,
        estimated_input_tokens: u64,
        estimated_output_tokens: Option<u64>,
    ) -> AiResult<Option<CostEstimate>> {
        let Some(price) = self.pricing.get_price_sync(provider, model) else {
            return Ok(None);
        };
        let input = per_million(price.input_per_million, estimated_input_tokens);
        let (output, output_is_upper_bound) = match estimated_output_tokens {
            Some(n) => (per_million(price.output_per_million, n), true),
            None => (Decimal::ZERO, false),
        };
        Ok(Some(CostEstimate {
            input,
            output,
            total: input + output,
            currency: Currency::usd(),
            output_is_upper_bound,
        }))
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

fn cost_from_pricing(price: &ModelPricing, usage: &Usage) -> Cost {
    let cached = usage.cached_tokens.unwrap_or(0);
    let billable_input = usage.prompt_tokens.saturating_sub(cached);
    let input_cost = per_million(price.input_per_million, billable_input);
    let cache_cost = per_million(price.cached_input_per_million, cached);
    let output_cost = per_million(price.output_per_million, usage.completion_tokens);
    Cost {
        currency: Currency::usd(),
        amount: input_cost + output_cost + cache_cost,
        input_cost,
        output_cost,
        cache_cost,
    }
}

fn per_million(rate: Option<Decimal>, tokens: u64) -> Decimal {
    match rate {
        Some(r) if tokens > 0 => r * Decimal::from(tokens) / Decimal::from(1_000_000u64),
        _ => Decimal::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn calculates_from_usage() {
        let price = ModelPricing {
            provider: ProviderId::deepseek(),
            model: ModelId::new("deepseek-chat"),
            input_per_million: Some(Decimal::new(14, 2)), // $0.14
            output_per_million: Some(Decimal::new(28, 2)),
            cached_input_per_million: Some(Decimal::new(14, 3)),
            effective_from: Utc::now(),
        };
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        let cost = cost_from_pricing(&price, &usage);
        assert_eq!(cost.input_cost, Decimal::new(14, 2));
        assert_eq!(cost.output_cost, Decimal::new(14, 2)); // 0.28 * 0.5
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
