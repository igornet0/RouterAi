//! RouterAi errors (never embed secrets).

use thiserror::Error;

/// Result alias.
pub type RouterResult<T> = Result<T, RouterError>;

/// Platform error.
#[derive(Debug, Error)]
pub enum RouterError {
    /// Invalid user/config input.
    #[error("invalid request: {0}")]
    Invalid(String),

    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Handler/agent disabled or kill switch.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Condition or policy failed.
    #[error("policy: {0}")]
    Policy(String),

    /// Tool permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Budget exceeded for a run/agent.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// Run cancelled.
    #[error("cancelled")]
    Cancelled,

    /// Timeout.
    #[error("timeout")]
    Timeout,

    /// Persistence.
    #[error("storage: {0}")]
    Storage(String),

    /// Tool execution failure.
    #[error("tool: {0}")]
    Tool(String),

    /// Upstream model / universal-ai failure.
    #[error("model: {0}")]
    Model(String),

    /// Serialization.
    #[error("serialization: {0}")]
    Serialization(String),

    /// Internal.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<serde_json::Error> for RouterError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

impl From<universal_ai::AiError> for RouterError {
    fn from(value: universal_ai::AiError) -> Self {
        Self::Model(value.to_string())
    }
}
