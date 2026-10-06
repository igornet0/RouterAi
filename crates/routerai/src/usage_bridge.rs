//! Bridge types from universal-ai usage/cost into runs.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Token usage on a run (subset of universal-ai Usage).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunUsage {
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Total.
    pub total_tokens: u64,
}

impl From<&universal_ai::Usage> for RunUsage {
    fn from(u: &universal_ai::Usage) -> Self {
        Self {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }
    }
}

/// Cost amount for a run step / total.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunCost {
    /// USD amount.
    pub amount: Decimal,
}

impl From<&universal_ai::Cost> for RunCost {
    fn from(c: &universal_ai::Cost) -> Self {
        Self { amount: c.amount }
    }
}
