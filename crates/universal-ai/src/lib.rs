//! Universal AI — provider-agnostic AI runtime layer.
//!
//! OpenAI-compatible HTTP is an adapter, not the internal model.
//! Accounts, keys, balance, usage, cost, routing, and limits are first-class.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod account;
pub mod balance;
pub mod capability;
pub mod client;
pub mod config;
pub mod cost;
pub mod error;
pub mod events;
pub mod health;
pub mod http;
pub mod models;
pub mod pricing;
pub mod provider;
pub mod provider_catalog;
pub mod providers;
pub mod rate_limit;
pub mod retry;
pub mod router;
pub mod secrets;
pub mod storage;
pub mod telemetry;
pub mod types;
pub mod usage;

pub use account::{
    Account, AccountManager, AccountStatus, AddKeyRequest, ApiKeyInfo, ApiKeyManager, KeyStatus,
};
pub use balance::{
    Balance, BalanceEvent, BalanceManager, BalanceMonitor, BalanceMonitorConfig, BalanceProbeStatus,
    BalanceSource, ProviderBalanceReport,
};
pub use capability::{Capability, ModelCapabilities, ProviderCapabilities};
pub use client::{AiClient, AiClientBuilder, ChatBuilder};
pub use config::{AiConfig, BudgetPolicy};
pub use cost::{Cost, CostEstimate, CostManager};
pub use error::{AiError, AiResult, ProviderErrorDetails};
pub use events::{AiEvent, EventBus};
pub use health::{HealthMonitor, HealthStatus};
pub use models::{ModelInfo, ModelRegistry};
pub use pricing::{
    CustomPricing, ModelPricing, PricingProvider, PricingRegistry, RemotePricing, StaticPricing,
};
pub use provider::Provider;
pub use provider_catalog::{build_provider, provider_catalog, ProviderDescriptor};
pub use providers::{
    Anthropic, DeepSeek, Gemini, OpenAI, OpenAICompatible, OpenAICompatibleBuilder, OpenRouter,
};
pub use rate_limit::RateLimit;
pub use retry::RetryPolicy;
pub use router::{KeySelectionStrategy, MaxCost, Router, TaskType};
pub use secrets::{
    FileSecretStore, KeychainSecretStore, MemorySecretStore, SecretStore, SecretString,
};
pub use storage::{MemoryStorage, Storage};
pub use types::{
    AccountId, ChatRequest, ChatResponse, ChatStream, Content, ContentPart, Currency, FinishReason,
    KeyId, Message, Metadata, ModelId, ProviderId, RequestId, ResponseFormat, Role, StreamEvent,
    Tool, ToolCall,
};
pub use usage::{
    validate_importance, RequestUsage, Usage, UsageManager, UsageReport, UsageRequest,
    UsageStatistics,
};

#[cfg(feature = "sqlite")]
pub use storage::SqliteStorage;
