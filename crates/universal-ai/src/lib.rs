//! Universal AI — provider-agnostic AI client runtime with fail-closed cost control.
//!
//! OpenAI-compatible HTTP is an adapter, not the internal model. Accounts, keys,
//! usage, cost, budgets, routing and limits are first-class.
//!
//! # Financial model in one paragraph
//!
//! Every model request goes through [`AiClient::chat`]. Each **physical attempt**
//! (first try, retry, fallback) gets its own accounting row: a worst-case
//! **estimate** is computed, **reserved** in the spend ledger (persisted before
//! anything is sent), and **settled** exactly once — to the actual cost when usage
//! and pricing are known, otherwise the reservation stays charged. Unknown cost is
//! never zero; with a budget active, a request whose worst case cannot be bounded
//! is rejected before any HTTP. See the `docs/` directory of this crate for
//! BUDGETS, PRICING, STREAMING, RETRY_AND_FALLBACK, STORAGE and more.
//!
//! # Example: budgeted request
//!
//! ```no_run
//! use rust_decimal::Decimal;
//! use universal_ai::{AiClient, BudgetPolicy, ErrorKind, ModelPricing, OpenAI, ProviderId};
//!
//! # async fn run() -> universal_ai::AiResult<()> {
//! let client = AiClient::builder()
//!     .provider(OpenAI::new("sk-...")?)
//!     .budget(BudgetPolicy::daily_usd(Decimal::new(5, 0)))
//!     .build()?;
//! client.pricing().upsert(ModelPricing::per_million(
//!     ProviderId::openai(),
//!     "gpt-4o-mini",
//!     Decimal::new(15, 2), // $0.15 / 1M input
//!     Decimal::new(60, 2), // $0.60 / 1M output
//! ));
//!
//! match client.chat().model("gpt-4o-mini").message("Hello").max_tokens(200).send().await {
//!     Ok(response) => println!("{} (cost: {:?})", response.text(), response.cost()),
//!     Err(err) => match err.kind() {
//!         ErrorKind::Budget | ErrorKind::Pricing => eprintln!("refused before sending: {err}"),
//!         ErrorKind::RateLimit | ErrorKind::Network | ErrorKind::Timeout => eprintln!("transient: {err}"),
//!         _ => eprintln!("failed: {err}"),
//!     },
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Example: explainable estimate (no network)
//!
//! ```
//! use rust_decimal::Decimal;
//! use universal_ai::{CostManager, ModelId, ModelPricing, PricingRegistry, ProviderId};
//!
//! let pricing = PricingRegistry::new();
//! pricing.upsert(ModelPricing::per_million(
//!     ProviderId::openai(),
//!     "m",
//!     Decimal::new(1000, 0),
//!     Decimal::new(2000, 0),
//! ));
//! let costs = CostManager::new(pricing);
//! let estimate = costs
//!     .estimate(&ProviderId::openai(), &ModelId::new("m"), 18, Some(100))
//!     .unwrap()
//!     .expect("priced model");
//! assert_eq!(estimate.total, Decimal::new(218, 3)); // 18 × $1000/M + 100 × $2000/M
//! assert!(estimate.explain().contains("worst-case: 0.218"));
//! ```
//!
//! # Example: metered streaming with cancellation by drop
//!
//! ```no_run
//! use futures::StreamExt;
//! use universal_ai::{AiClient, StreamEvent};
//!
//! # async fn run(client: &AiClient) -> universal_ai::AiResult<()> {
//! let mut stream = client.chat().model("m").message("Tell a story").max_tokens(500).stream().await?;
//! while let Some(event) = stream.next().await {
//!     match event? {
//!         StreamEvent::TextDelta { text } => print!("{text}"),
//!         StreamEvent::Usage { usage } => eprintln!("\nusage: {usage:?}"),
//!         _ => {}
//!     }
//! }
//! // Dropping `stream` early closes the connection and settles the attempt as
//! // `Abandoned` (actual cost if final usage arrived, otherwise the reservation).
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod account;
pub mod balance;
pub mod budget;
pub mod capability;
pub mod client;
pub mod config;
pub mod cost;
pub mod error;
pub mod events;
pub mod execution;
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
    Balance, BalanceEvent, BalanceManager, BalanceMonitor, BalanceMonitorConfig,
    BalanceProbeStatus, BalanceSource, ProviderBalanceReport,
};
pub use budget::{BudgetScope, BudgetStatus};
pub use capability::{Capability, ModelCapabilities, ProviderCapabilities};
pub use client::{AiClient, AiClientBuilder, ChatBuilder, CostApi, ProviderSummary};
pub use config::{AiConfig, BudgetPolicy, MissingUsagePolicy};
pub use cost::{Cost, CostEstimate, CostManager, EstimateBreakdown, OutputBoundSource, PricingGap};
pub use error::{AiError, AiResult, ErrorKind, ProviderErrorDetails};
pub use events::{AiEvent, EventBus};
pub use health::{HealthMonitor, HealthStatus};
pub use models::{ModelInfo, ModelRegistry};
pub use pricing::{
    CustomPricing, ModelPricing, PriceTier, PricingProvider, PricingRegistry, RateSource,
    RemotePricing, StaticPricing, TierMode, TierRates, Tiering, TokenClass,
};
pub use provider::{Provider, ProviderCredential};
pub use provider_catalog::{
    build_provider, build_provider_template, provider_catalog, ProviderDescriptor,
};
pub use providers::{
    Anthropic, DeepSeek, Gemini, OpenAI, OpenAICompatible, OpenAICompatibleBuilder, OpenRouter,
    OutputLimitParam,
};
pub use rate_limit::RateLimit;
pub use retry::RetryPolicy;
pub use router::{KeySelectionStrategy, MaxCost, Router, TaskType};
#[allow(deprecated)]
pub use secrets::FileSecretStore;
pub use secrets::{
    EncryptedFileSecretStore, KeychainSecretStore, MemorySecretStore, SecretStore, SecretStoreKey,
    SecretString,
};
pub use storage::{MemoryStorage, SpendLimits, Storage};
pub use telemetry::{AttemptReport, TelemetrySink};
pub use types::{
    AccountId, ChatRequest, ChatResponse, ChatStream, Content, ContentPart, Currency, FinishReason,
    FunctionCall, KeyId, Message, Metadata, ModelId, ProviderId, RequestId, ResponseFormat, Role,
    StreamEvent, Tool, ToolCall, ToolFunction, ToolResult,
};
pub use usage::{
    validate_importance, CostAccounting, CostStatus, Reconciliation, RequestUsage, TokenBreakdown,
    Usage, UsageManager, UsageReport, UsageRequest, UsageStatistics, DEFAULT_RECENT_ATTEMPTS,
};

#[cfg(feature = "sqlite")]
pub use storage::SqliteStorage;
