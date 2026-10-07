//! AiClient — public unified API.
//!
//! Every model request goes through one path: [`ChatBuilder::send`] /
//! [`ChatBuilder::stream`] → `AiClient::execute` (provider selection, retry and
//! fallback policy) → `AiClient::execute_attempt` (one physical attempt: budget
//! gate, dispatch, settlement via [`crate::execution`]).

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use rust_decimal::Decimal;
use tracing::Instrument;

use crate::account::{AccountManager, ApiKeyInfo, ApiKeyManager};
use crate::balance::{Balance, BalanceManager, BalanceMonitor, BalanceMonitorConfig};
use crate::budget::{
    input_bound, output_bound, worst_case_cost, BudgetScope, BudgetStatus, RequestBudget,
    SpendLedger,
};
use crate::capability::{Capability, ProviderCapabilities};
use crate::config::MissingUsagePolicy;
use crate::config::{AiConfig, BudgetPolicy};
use crate::cost::{CostEstimate, CostManager, OutputBoundSource};
use crate::error::{AiError, AiResult};
use crate::events::{AiEvent, EventBus, RequestFailed, RequestStarted};
use crate::execution::{metered_stream, AttemptMeter, Core};
use crate::health::{HealthMonitor, HealthStatus};
use crate::http::{HttpClient, HttpConfig};
use crate::models::{ModelRegistry, ModelsApi};
use crate::pricing::PricingRegistry;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::provider_catalog::build_provider_template;
use crate::router::Router;
use crate::secrets::{MemorySecretStore, SecretStore};
use crate::storage::{MemoryStorage, SpendLimits, Storage};
use crate::telemetry::{NoopTelemetry, TelemetrySink};
use crate::types::{
    AccountId, ChatRequest, ChatResponse, ChatStream, Content, ContentPart, KeyId, Message,
    ModelId, ProviderId, RequestId, Tool,
};
use crate::usage::{
    CostAccounting, CostStatus, RequestUsage, Usage, UsageManager, UsageStatistics,
};

/// Primary entry point.
pub struct AiClient {
    providers: RwLock<Vec<DynProvider>>,
    http: HttpClient,
    config: AiConfig,
    events: Arc<EventBus>,
    accounts: AccountManager,
    keys: Arc<ApiKeyManager>,
    balances: Arc<BalanceManager>,
    usage: Arc<UsageManager>,
    cost: Arc<CostManager>,
    models: Arc<ModelRegistry>,
    health: Arc<HealthMonitor>,
    storage: Arc<dyn Storage>,
    ledger: Arc<SpendLedger>,
    telemetry: Arc<dyn TelemetrySink>,
    core: Arc<Core>,
    default_account: AccountId,
    /// Serializes reconciliations (read-modify-write of one row).
    reconcile_lock: tokio::sync::Mutex<()>,
}

/// Id and capabilities of a configured provider (no adapter handle, so no way
/// to send a request around the budget / accounting path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    /// Provider id.
    pub id: ProviderId,
    /// Advertised capabilities.
    pub capabilities: ProviderCapabilities,
}

/// Builder for [`AiClient`].
pub struct AiClientBuilder {
    providers: Vec<DynProvider>,
    config: AiConfig,
    http_config: HttpConfig,
    secret_store: Option<Arc<dyn SecretStore>>,
    storage: Option<Arc<dyn Storage>>,
    telemetry: Option<Arc<dyn TelemetrySink>>,
    load_example_prices: bool,
    allow_empty_providers: bool,
    recent_attempts: usize,
}

impl Default for AiClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AiClientBuilder {
    /// Start building.
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            config: AiConfig::default(),
            http_config: HttpConfig::default(),
            secret_store: None,
            storage: None,
            telemetry: None,
            load_example_prices: false,
            allow_empty_providers: false,
            recent_attempts: crate::usage::DEFAULT_RECENT_ATTEMPTS,
        }
    }

    /// Add a provider adapter.
    pub fn provider(mut self, provider: impl Provider + 'static) -> Self {
        self.providers.push(Arc::new(provider));
        self
    }

    /// Allow building with zero providers (keys registered later at runtime).
    pub fn allow_empty_providers(mut self) -> Self {
        self.allow_empty_providers = true;
        self
    }

    /// Enable safe fallback across providers.
    pub fn fallback(mut self, enabled: bool) -> Self {
        self.config.fallback = enabled;
        self
    }

    /// Set config.
    pub fn config(mut self, config: AiConfig) -> Self {
        self.config = config;
        self
    }

    /// Budget policy.
    pub fn budget(mut self, budget: BudgetPolicy) -> Self {
        self.config.budget = budget;
        self
    }

    /// HTTP config.
    pub fn http_config(mut self, cfg: HttpConfig) -> Self {
        self.http_config = cfg;
        self
    }

    /// Secret store backend.
    pub fn secret_store(mut self, store: Arc<dyn SecretStore>) -> Self {
        self.secret_store = Some(store);
        self
    }

    /// Persistence backend.
    pub fn storage(mut self, storage: Arc<dyn Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Telemetry (opt-in).
    pub fn telemetry(mut self, sink: Arc<dyn TelemetrySink>) -> Self {
        self.config.telemetry_enabled = true;
        self.telemetry = Some(sink);
        self
    }

    /// How many recent attempt rows to keep in memory (default
    /// [`crate::usage::DEFAULT_RECENT_ATTEMPTS`]); statistics are unaffected.
    pub fn recent_attempts(mut self, capacity: usize) -> Self {
        self.recent_attempts = capacity;
        self
    }

    /// Load bundled example prices into the registry (explicit opt-in).
    pub fn with_example_prices(mut self) -> Self {
        self.load_example_prices = true;
        self
    }

    /// Build client.
    pub fn build(self) -> AiResult<AiClient> {
        if self.providers.is_empty() && !self.allow_empty_providers {
            return Err(AiError::Config {
                message: "at least one provider is required".into(),
            });
        }
        let http = HttpClient::new(self.http_config)?;
        let events = Arc::new(EventBus::new(256));
        let store = self
            .secret_store
            .unwrap_or_else(|| Arc::new(MemorySecretStore::new()));
        let storage = self
            .storage
            .unwrap_or_else(|| Arc::new(MemoryStorage::new()));
        let telemetry: Arc<dyn TelemetrySink> = if self.config.telemetry_enabled {
            self.telemetry.unwrap_or_else(|| Arc::new(NoopTelemetry))
        } else {
            Arc::new(NoopTelemetry)
        };

        let pricing = PricingRegistry::new();
        if self.load_example_prices {
            pricing.load_example_prices();
        }

        let models = Arc::new(ModelRegistry::new());
        let keys = Arc::new(ApiKeyManager::new(store));
        let usage = Arc::new(UsageManager::with_capacity(self.recent_attempts));
        let cost = Arc::new(CostManager::new(pricing));
        let ledger = Arc::new(SpendLedger::new(Arc::clone(&storage)));
        let core = Arc::new(Core {
            ledger: Arc::clone(&ledger),
            keys: Arc::clone(&keys),
            usage: Arc::clone(&usage),
            cost: Arc::clone(&cost),
            events: Arc::clone(&events),
            telemetry: Arc::clone(&telemetry),
            store_content: self.config.store_request_content,
            unbounded: Default::default(),
        });

        Ok(AiClient {
            providers: RwLock::new(self.providers),
            http,
            config: self.config,
            events: Arc::clone(&events),
            accounts: AccountManager::new(),
            keys,
            balances: Arc::new(BalanceManager::new()),
            usage,
            cost,
            models,
            health: Arc::new(HealthMonitor::new(Some(events))),
            ledger,
            storage,
            telemetry,
            core,
            default_account: AccountId::new("default"),
            reconcile_lock: tokio::sync::Mutex::new(()),
        })
    }
}

impl AiClient {
    /// Builder entry.
    pub fn builder() -> AiClientBuilder {
        AiClientBuilder::new()
    }

    /// Fluent chat API — the entry point for every model request.
    pub fn chat(&self) -> ChatBuilder<'_> {
        ChatBuilder {
            client: self,
            model: None,
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            side_effecting: false,
            preferred_provider: None,
            tools: Vec::new(),
            options: RequestOptions::default(),
        }
    }

    /// Models API.
    pub fn models(&self) -> ModelsApi<'_> {
        ModelsApi {
            registry: &self.models,
        }
    }

    /// Refresh models from providers into registry.
    pub async fn discover_models(&self) -> AiResult<Vec<crate::models::ModelInfo>> {
        let mut all = Vec::new();
        for p in self.provider_snapshot() {
            if !p.supports(Capability::ModelList) {
                continue;
            }
            let listed = match self.bind_provider(&p).await {
                Ok(binding) => binding.provider.list_models().await,
                Err(err) => Err(err),
            };
            match listed {
                Ok(list) => {
                    for m in list {
                        self.events.emit(AiEvent::ModelDiscovered(m.clone()));
                        self.models.upsert(m.clone());
                        all.push(m);
                    }
                }
                Err(err) => {
                    tracing::warn!(provider = %p.id(), error = %err, "model discovery failed");
                }
            }
        }
        Ok(all)
    }

    /// Account manager.
    pub fn accounts(&self) -> &AccountManager {
        &self.accounts
    }

    /// Key manager.
    pub fn keys(&self) -> &ApiKeyManager {
        &self.keys
    }

    /// Balance helpers.
    pub fn balances(&self) -> &BalanceManager {
        &self.balances
    }

    /// Fetch balance for a provider/account (account id reserved for multi-account mapping).
    pub async fn balance_for(&self, provider: &ProviderId) -> AiResult<Option<Balance>> {
        let p = self
            .provider_snapshot()
            .into_iter()
            .find(|p| &p.id() == provider)
            .ok_or_else(|| AiError::InvalidRequest {
                message: format!("provider not configured: {provider}"),
            })?;
        if !p.supports(Capability::Balance) {
            return Err(AiError::UnsupportedCapability {
                capability: Capability::Balance,
                provider: Some(provider.clone()),
            });
        }
        let bal = self.bind_provider(&p).await?.provider.balance().await?;
        if let Some(b) = &bal {
            self.balances.save(b.clone()).await;
            let _ = self.storage.save_balance(b).await;
        }
        Ok(bal)
    }

    /// Probe balances for all registered providers (DeepSeek live + OpenAI workarounds).
    pub async fn probe_balances(&self) -> Vec<crate::balance::ProviderBalanceReport> {
        use crate::balance::{BalanceProbeStatus, BalanceSource, ProviderBalanceReport};
        use crate::usage::UsageRequest;

        let now = Utc::now();
        let mut out = Vec::new();
        let accounts = self.accounts.list_accounts().await;

        for provider in self.provider_snapshot() {
            let provider_id = provider.id();
            // Probe with the same key selection as model requests.
            let provider = match self.bind_provider(&provider).await {
                Ok(binding) => binding.provider,
                Err(_) => provider,
            };
            let tracked = self.usage.statistics_by_provider(&provider_id).total_cost;
            let account = accounts.iter().find(|a| a.provider == provider_id).cloned();
            let credit_budget = account.as_ref().and_then(|a| a.credit_budget);
            let account_id = account.as_ref().map(|a| a.id.clone());

            // 1) Live provider balance when supported.
            if provider.supports(Capability::Balance) {
                match provider.balance().await {
                    Ok(Some(bal)) => {
                        self.balances.save(bal.clone()).await;
                        out.push(ProviderBalanceReport {
                            provider: provider_id,
                            status: BalanceProbeStatus::Ok,
                            source: BalanceSource::ProviderApi,
                            currency: Some(bal.currency.clone()),
                            total: Some(bal.total),
                            available: bal.available.or(Some(bal.total)),
                            period_spend: None,
                            tracked_spend: Some(tracked),
                            credit_budget,
                            account_id,
                            message: None,
                            updated_at: bal.updated_at,
                        });
                        continue;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        // Fall through to budget / costs workarounds.
                    }
                }
            }

            // 2) Manual credit budget → estimated remaining.
            if let Some(budget) = credit_budget {
                let available = budget - tracked;
                out.push(ProviderBalanceReport {
                    provider: provider_id.clone(),
                    status: BalanceProbeStatus::Estimated,
                    source: BalanceSource::ManualBudget,
                    currency: Some(crate::types::Currency::usd()),
                    total: Some(budget),
                    available: Some(available),
                    period_spend: None,
                    tracked_spend: Some(tracked),
                    credit_budget: Some(budget),
                    account_id: account_id.clone(),
                    message: Some(
                        "Estimated from credit budget minus RouterAi tracked spend \
                         (OpenAI does not expose prepaid balance via project API keys)"
                            .into(),
                    ),
                    updated_at: now,
                });
                continue;
            }

            // 3) OpenAI Organization Costs API (Admin key) — period spend only.
            if provider.supports(Capability::Usage) {
                let end = now;
                let start = now - chrono::Duration::days(30);
                match provider.usage(UsageRequest { start, end }).await {
                    Ok(report) => {
                        out.push(ProviderBalanceReport {
                            provider: provider_id,
                            status: BalanceProbeStatus::Ok,
                            source: BalanceSource::CostsApi,
                            currency: Some(crate::types::Currency::usd()),
                            total: None,
                            available: None,
                            period_spend: report.cost,
                            tracked_spend: Some(tracked),
                            credit_budget: None,
                            account_id,
                            message: Some(
                                "OpenAI prepaid balance is not available via API keys. \
                                 Showing last-30d org costs (Admin key). \
                                 Set a credit budget on the account for estimated remaining."
                                    .into(),
                            ),
                            updated_at: now,
                        });
                        continue;
                    }
                    Err(err) => {
                        out.push(ProviderBalanceReport {
                            provider: provider_id,
                            status: BalanceProbeStatus::Unsupported,
                            source: BalanceSource::None,
                            currency: None,
                            total: None,
                            available: None,
                            period_spend: None,
                            tracked_spend: Some(tracked),
                            credit_budget: None,
                            account_id,
                            message: Some(format!(
                                "No prepaid balance API. Tracked spend ${tracked}. \
                                 Set a credit budget in Settings, or add an OpenAI Admin key \
                                 for org costs. Detail: {err}"
                            )),
                            updated_at: now,
                        });
                        continue;
                    }
                }
            }

            out.push(ProviderBalanceReport {
                provider: provider_id,
                status: BalanceProbeStatus::Unsupported,
                source: BalanceSource::None,
                currency: None,
                total: None,
                available: None,
                period_spend: None,
                tracked_spend: Some(tracked),
                credit_budget: None,
                account_id,
                message: Some(format!(
                    "Provider does not expose balance. Locally tracked spend: ${tracked}"
                )),
                updated_at: now,
            });
        }

        out
    }

    /// Cost manager.
    pub fn cost(&self) -> CostApi<'_> {
        CostApi { client: self }
    }

    /// Usage / stats.
    pub fn stats(&self) -> StatsApi<'_> {
        StatsApi { client: self }
    }

    /// Event bus.
    pub fn events(&self) -> EventApi<'_> {
        EventApi { bus: &self.events }
    }

    /// List persisted AI request rows (newest first).
    pub async fn list_ai_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>> {
        self.storage.list_requests(limit).await
    }

    /// Accounting row of a request attempt made by this client (in memory):
    /// usage, key, estimated / actual / charged cost, budget decision.
    pub fn request_usage(&self, id: &RequestId) -> Option<RequestUsage> {
        self.usage.get(id)
    }

    /// Every attempt (retries, fallbacks, rejections) of the logical request that
    /// `attempt_id` belongs to, in attempt order, from this process's recent rows
    /// (bounded; see [`UsageManager`]). Sum their [`RequestUsage::budget_charge`]
    /// for what the logical request cost — `ChatResponse::request_id` identifies
    /// only the attempt that answered. For an authoritative answer (older
    /// requests, other processes, after a restart) use
    /// [`AiClient::load_logical_request_attempts`].
    pub fn logical_request_attempts(&self, attempt_id: &RequestId) -> Vec<RequestUsage> {
        let Some(logical) = self
            .usage
            .get(attempt_id)
            .map(|r| r.accounting.logical_request_id.unwrap_or(r.request_id))
        else {
            return Vec::new();
        };
        self.usage.attempts_of(&logical)
    }

    /// Every persisted attempt of the logical request that `attempt_id` belongs
    /// to, in attempt order — the storage's view, independent of this process's
    /// memory. Empty when `attempt_id` is unknown.
    pub async fn load_logical_request_attempts(
        &self,
        attempt_id: &RequestId,
    ) -> AiResult<Vec<RequestUsage>> {
        let Some(row) = self.storage.get_request(attempt_id).await? else {
            return Ok(Vec::new());
        };
        let logical = row.accounting.logical_request_id.unwrap_or(row.request_id);
        self.storage.list_attempts(&logical).await
    }

    /// Correct a settled attempt to what the provider later reported or billed
    /// (usage export, costs API, invoice): the attempt's charge moves to the
    /// reconciled amount and the budget ledger by the difference — a refund when
    /// the attempt was charged its worst case, a surcharge when it was charged
    /// less. The row becomes [`CostStatus::Reconciled`]; its `cost_note` keeps the
    /// source, the previous status and charge, and the adjustment.
    ///
    /// Refused for attempts still `Pending` (in flight — or orphaned: run
    /// [`AiClient::recover_orphaned_reservations`] first) and for `Rejected` ones
    /// (never sent). Usage that cannot be priced is `PricingUnavailable`; the
    /// amount must not be negative and the source must not be empty.
    /// Reconciling again with the same amount and source changes nothing.
    pub async fn reconcile_attempt(
        &self,
        attempt_id: &RequestId,
        with: crate::usage::Reconciliation,
    ) -> AiResult<RequestUsage> {
        use crate::usage::Reconciliation;
        let _serial = self.reconcile_lock.lock().await;
        let mut row =
            self.storage
                .get_request(attempt_id)
                .await?
                .ok_or_else(|| AiError::NotFound {
                    message: format!("request {attempt_id}"),
                })?;
        let invalid = |message: String| Err(AiError::InvalidRequest { message });
        match row.accounting.status {
            CostStatus::Pending => {
                return invalid(format!(
                    "attempt {attempt_id} is still pending (in flight or orphaned); \
                     recover orphaned reservations before reconciling"
                ))
            }
            CostStatus::Rejected => return invalid(format!("attempt {attempt_id} was never sent")),
            _ => {}
        }
        let (cost, usage, source) = match with {
            Reconciliation::Usage { usage, source } => {
                let cost = self
                    .cost
                    .price(&row.provider, &row.model, &usage)
                    .map_err(|_| AiError::PricingUnavailable {
                        provider: row.provider.clone(),
                        model: row.model.clone(),
                    })?;
                (cost, Some(usage), source)
            }
            Reconciliation::Amount { amount, source } => {
                if amount.is_sign_negative() {
                    return invalid(format!("reconciled amount {amount} is negative"));
                }
                let cost = crate::cost::Cost {
                    amount,
                    ..crate::cost::Cost::zero()
                };
                (cost, None, source)
            }
        };
        let source = source.trim().to_string();
        if source.is_empty() {
            return invalid("reconciliation source is required".into());
        }
        let note_head = format!("reconciled from {source}:");
        let previous = row.budget_charge();
        if row.accounting.status == CostStatus::Reconciled
            && row.accounting.charged_cost == Some(cost.amount)
            && row
                .accounting
                .cost_note
                .as_deref()
                .is_some_and(|n| n.starts_with(&note_head))
        {
            return Ok(row);
        }
        let adjustment = cost.amount - previous;
        row.accounting.cost_note = Some(format!(
            "{note_head} previously {:?} charged {previous}; adjustment {adjustment}",
            row.accounting.status
        ));
        row.accounting.status = CostStatus::Reconciled;
        row.accounting.charged_cost = Some(cost.amount);
        row.cost = Some(cost);
        if let Some(usage) = usage {
            row.usage = usage;
        }
        self.ledger.settle(previous, &row).await?;
        self.usage.replace(row.clone());
        tracing::info!(
            target: "universal_ai::accounting",
            attempt_id = %row.request_id,
            provider = %row.provider,
            model = %row.model,
            previous_charge = %previous,
            charged_cost = ?row.accounting.charged_cost,
            adjustment = %adjustment,
            source = %source,
            "attempt reconciled"
        );
        Ok(row)
    }

    /// Committed spend (settled + reserved) for the current UTC day and month,
    /// globally (`None`) or for a budget scope such as `agent:<id>`.
    pub async fn budget_status(&self, scope: Option<&str>) -> AiResult<BudgetStatus> {
        self.ledger.status(scope).await
    }

    /// Mark reservations left `Pending` by a previous process (crash / kill before
    /// settlement) as [`CostStatus::Abandoned`]. Their charge is **kept** — a
    /// possibly billed request never becomes free — only the status changes, so
    /// audits can tell orphaned reservations from in-flight ones. Only rows started
    /// before `started_before` are touched; pass this process's start time.
    /// Returns the number of rows updated.
    pub async fn recover_orphaned_reservations(
        &self,
        started_before: chrono::DateTime<Utc>,
    ) -> AiResult<u64> {
        let n = self.storage.abandon_pending(started_before).await?;
        if n > 0 {
            tracing::warn!(
                rows = n,
                "orphaned pending reservations marked abandoned (charge kept)"
            );
        }
        Ok(n)
    }

    /// Load one persisted AI request by id.
    pub async fn get_ai_request(&self, id: &RequestId) -> AiResult<Option<RequestUsage>> {
        self.storage.get_request(id).await
    }

    /// Set optional importance rating (0–10) on a persisted request.
    pub async fn set_request_importance(
        &self,
        id: &RequestId,
        importance: Option<u8>,
    ) -> AiResult<RequestUsage> {
        self.storage.set_importance(id, importance).await
    }

    /// Health monitor.
    pub fn health(&self) -> &HealthMonitor {
        &self.health
    }

    /// Configured provider adapters (snapshot).
    ///
    /// **Escape hatch:** calling `chat` / `stream_chat` on an adapter directly
    /// bypasses budgets, reservations, retries and accounting. Use
    /// [`AiClient::provider_summaries`] for ids / capabilities and
    /// [`AiClient::chat`] for requests.
    #[deprecated(
        since = "0.1.0",
        note = "direct adapter calls bypass budget and accounting; use provider_summaries() / chat()"
    )]
    pub fn providers(&self) -> Vec<DynProvider> {
        self.provider_snapshot()
    }

    fn provider_snapshot(&self) -> Vec<DynProvider> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ids and capabilities of the configured providers.
    pub fn provider_summaries(&self) -> Vec<ProviderSummary> {
        self.provider_snapshot()
            .iter()
            .map(|p| ProviderSummary {
                id: p.id(),
                capabilities: p.capabilities(),
            })
            .collect()
    }

    /// Run a health probe for one configured provider (with its selected key).
    /// Probes never send model requests.
    pub async fn check_provider_health(&self, id: &ProviderId) -> AiResult<HealthStatus> {
        let provider = self
            .provider_snapshot()
            .into_iter()
            .find(|p| &p.id() == id)
            .ok_or_else(|| AiError::NotFound {
                message: format!("provider {id}"),
            })?;
        let bound = match self.bind_provider(&provider).await {
            Ok(binding) => binding.provider,
            Err(_) => provider,
        };
        self.health.check(&bound).await
    }

    /// Register or replace a provider adapter by id.
    pub fn register_provider(&self, provider: DynProvider) {
        let id = provider.id();
        let mut g = self.providers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = g.iter().position(|p| p.id() == id) {
            g[pos] = provider;
        } else {
            g.push(provider);
        }
        tracing::info!(provider = %id, "provider registered");
    }

    /// Remove a provider adapter by id.
    pub fn unregister_provider(&self, id: &ProviderId) {
        let mut g = self.providers.write().unwrap_or_else(|e| e.into_inner());
        g.retain(|p| &p.id() != id);
    }

    /// Ensure a provider adapter exists for a managed API key (+ optional base URL).
    ///
    /// The registered adapter is keyless: each request binds the key selected for it
    /// (see [`crate::Provider::with_credential`]), so adding, rotating or deleting keys
    /// never leaves a stale secret inside the provider.
    pub async fn sync_provider_from_key(
        &self,
        info: &ApiKeyInfo,
        base_url: Option<&str>,
    ) -> AiResult<()> {
        if self.keys.get_secret(&info.id).await?.is_none() {
            return Err(AiError::InvalidRequest {
                message: format!("secret missing for key {}", info.id),
            });
        }
        let provider = build_provider_template(&info.provider, base_url, self.http.clone())?;
        self.register_provider(provider);
        Ok(())
    }

    /// Shared HTTP client.
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// Balance monitor builder.
    pub fn monitor(&self) -> MonitorApi<'_> {
        MonitorApi { client: self }
    }

    /// Router (routing decisions only; requests go through [`AiClient::chat`]).
    pub fn router(&self) -> Router {
        Router::new(
            self.provider_snapshot(),
            Arc::clone(&self.models),
            Arc::clone(&self.cost),
            Arc::clone(&self.health),
        )
    }

    /// Pricing registry (runtime-updatable).
    pub fn pricing(&self) -> &PricingRegistry {
        self.cost.pricing()
    }

    fn resolve_providers_for_model(&self, request: &ChatRequest) -> AiResult<Vec<DynProvider>> {
        let providers = self.provider_snapshot();
        if providers.is_empty() {
            return Err(AiError::NoAvailableProvider {
                message: "no providers configured — add an API key first".into(),
            });
        }

        let preferred = request
            .preferred_provider
            .clone()
            .or_else(|| self.router().resolve_provider(request.model.as_str()).ok());

        let mut ordered = Vec::new();
        if let Some(id) = preferred {
            if let Some(p) = providers.iter().find(|p| p.id() == id) {
                ordered.push(Arc::clone(p));
            }
        }
        for p in &providers {
            if ordered.iter().any(|o| o.id() == p.id()) {
                continue;
            }
            ordered.push(Arc::clone(p));
        }
        Ok(ordered)
    }

    /// Providers able to serve `request` in `mode`, in routing order (preferred
    /// provider first). Capability, model-limit and health checks happen here —
    /// before anything is reserved or sent. Skipping an ineligible provider is not
    /// a fallback: nothing was attempted on it.
    async fn candidates(&self, request: &ChatRequest, mode: Mode) -> AiResult<Vec<DynProvider>> {
        let ordered = self.resolve_providers_for_model(request)?;
        let mut out = Vec::new();
        let mut first_reject = None;
        for p in ordered {
            match self.check_provider(&p, request, mode).await {
                Ok(()) => out.push(p),
                Err(err) => {
                    tracing::debug!(provider = %p.id(), error = %err, "provider not eligible");
                    first_reject.get_or_insert(err);
                }
            }
        }
        if out.is_empty() {
            return Err(
                first_reject.unwrap_or_else(|| AiError::NoAvailableProvider {
                    message: "no provider can serve this request".into(),
                }),
            );
        }
        Ok(out)
    }

    async fn check_provider(
        &self,
        provider: &DynProvider,
        request: &ChatRequest,
        mode: Mode,
    ) -> AiResult<()> {
        let id = provider.id();
        let unsupported = |capability| AiError::UnsupportedCapability {
            capability,
            provider: Some(id.clone()),
        };
        let mut needed = vec![Capability::Chat];
        if mode == Mode::Stream {
            needed.push(Capability::Streaming);
        }
        if !request.tools.is_empty() {
            needed.push(Capability::ToolCalling);
        }
        if request.response_format.is_some() {
            needed.push(Capability::StructuredOutput);
        }
        if let Some(missing) = needed.into_iter().find(|c| !provider.supports(*c)) {
            return Err(unsupported(missing));
        }
        // Model-level limits, when the model is registered for this provider.
        if let Some(info) = self.models.get(&format!("{id}:{}", request.model)) {
            if mode == Mode::Stream && !info.capabilities.streaming {
                return Err(unsupported(Capability::Streaming));
            }
            if !request.tools.is_empty() && !info.capabilities.tool_calling {
                return Err(unsupported(Capability::ToolCalling));
            }
            if let (Some(max), Some(requested)) = (info.max_output_tokens, request.max_tokens) {
                if u64::from(requested) > max {
                    return Err(AiError::InvalidRequest {
                        message: format!(
                            "max_tokens {requested} exceeds {id}/{} output limit {max}",
                            request.model
                        ),
                    });
                }
            }
        }
        if !self.health.is_healthy(&id).await {
            return Err(AiError::NoAvailableProvider {
                message: format!("provider {id} is unhealthy"),
            });
        }
        Ok(())
    }

    /// Whether this request must pass the fail-closed budget gate.
    fn budget_controlled(&self, budget: &RequestBudget) -> bool {
        self.config.budget.is_limited()
            || budget.require_cost_bound
            || budget.max_cost.is_some()
            || budget
                .scope
                .as_ref()
                .is_some_and(|s| s.daily_limit.is_some())
    }

    /// Accounting row for one physical attempt.
    fn attempt_row(
        &self,
        ctx: &AttemptCtx,
        provider: &ProviderId,
        binding: Option<&ProviderBinding>,
        request: &ChatRequest,
        request_json: &serde_json::Value,
        budget: &RequestBudget,
    ) -> RequestUsage {
        let now = Utc::now();
        RequestUsage {
            request_id: ctx.attempt_id,
            provider: provider.clone(),
            account: binding
                .map(|b| b.account_id.clone())
                .unwrap_or_else(|| self.default_account.clone()),
            api_key: binding.and_then(|b| b.key_id.clone()),
            model: request.model.clone(),
            started_at: now,
            finished_at: now,
            usage: Usage::default(),
            cost: None,
            success: false,
            latency_ms: 0,
            request_json: request_json.clone(),
            response_json: None,
            importance: None,
            accounting: CostAccounting {
                budget_scope: budget.scope.as_ref().map(|s| s.name.clone()),
                logical_request_id: Some(ctx.logical_id),
                attempt: ctx.seq,
                retry: ctx.retry,
                ..Default::default()
            },
        }
    }

    /// Budget gate for one physical attempt: worst-case estimate → per-request caps
    /// → atomic reservation in the spend ledger. Returns the reserved amount (zero
    /// when not budget-controlled) and the output token bound. For a controlled
    /// attempt whose bound comes from the model registry, that bound is pinned into
    /// the request as `max_tokens`, so the provider cannot exceed what was reserved.
    /// Nothing has been sent when this fails.
    async fn preflight(
        &self,
        provider_id: &ProviderId,
        request: &mut ChatRequest,
        budget: &RequestBudget,
        ctx: &AttemptCtx,
        row: &mut RequestUsage,
    ) -> AiResult<(Decimal, Option<u64>)> {
        let controlled = ctx.controlled;
        let bound = output_bound(&self.models, provider_id, request);
        let estimate = bound.as_ref().ok().and_then(|b| {
            self.cost
                .estimate_bounds(provider_id, &request.model, input_bound(request), Some(*b))
        });
        row.accounting.estimated_cost = estimate.as_ref().map(|e| e.total);
        if !controlled {
            return Ok((Decimal::ZERO, bound.ok().map(|(n, _)| n)));
        }
        if self.core.is_unbounded(provider_id, &request.model) {
            return Err(AiError::WorstCaseUnbounded {
                provider: provider_id.clone(),
                model: request.model.clone(),
            });
        }
        let (tokens, source) = bound?;
        let estimate = estimate.ok_or_else(|| AiError::PricingUnavailable {
            provider: provider_id.clone(),
            model: request.model.clone(),
        })?;
        if source == OutputBoundSource::ModelMaxOutput {
            let pinned = u32::try_from(tokens).unwrap_or(u32::MAX);
            request.max_tokens = Some(pinned);
            if let Some(obj) = row.request_json.as_object_mut() {
                obj.insert("max_tokens".into(), serde_json::json!(pinned));
            }
        }
        let total = estimate.total;
        tracing::debug!(
            provider = %provider_id,
            model = %request.model,
            estimate = %estimate.explain(),
            "worst-case estimate"
        );
        // Caps cover the whole logical request: earlier (retried / fallen back)
        // attempts' charges plus this attempt's worst case.
        let prior = ctx.prior_charged;
        let note = |max: Decimal, what: &str| {
            if prior.is_zero() {
                format!("worst-case cost {total} exceeds {what} {max}")
            } else {
                format!(
                    "worst-case cost {total} + {prior} already charged by earlier attempts \
                     exceeds {what} {max}"
                )
            }
        };
        if let Some(max) = self.config.budget.max_request_cost {
            if prior + total > max {
                return Err(AiError::BudgetExceeded {
                    message: note(max, "max_request_cost"),
                });
            }
        }
        if let Some(max) = budget.max_cost {
            if prior + total > max {
                return Err(AiError::BudgetExceeded {
                    message: note(max, "remaining budget"),
                });
            }
        }
        row.accounting.status = CostStatus::Pending;
        row.accounting.reserved_cost = Some(total);
        row.accounting.charged_cost = Some(total);
        let limits = SpendLimits {
            daily: self.config.budget.max_daily_cost,
            monthly: self.config.budget.max_monthly_cost,
            scope_daily: budget.scope.as_ref().and_then(|s| s.daily_limit),
        };
        self.ledger.reserve(row, limits).await?;
        Ok((total, Some(tokens)))
    }

    /// Record an attempt rejected before dispatch (audit trail, nothing charged).
    async fn record_rejection(&self, mut row: RequestUsage, err: &AiError) {
        tracing::warn!(
            provider = %row.provider,
            model = %row.model,
            key_id = ?row.api_key.as_ref().map(|k| k.to_string()),
            estimated_cost = ?row.accounting.estimated_cost,
            reason = err.budget_reason().unwrap_or("error"),
            error_kind = ?err.kind(),
            "request rejected before dispatch"
        );
        row.finished_at = Utc::now();
        row.accounting.status = CostStatus::Rejected;
        row.accounting.reserved_cost = None;
        row.accounting.charged_cost = Some(Decimal::ZERO);
        row.accounting.rejection = Some(err.to_string());
        row.accounting.error_kind = Some(err.kind());
        self.core.finish(Decimal::ZERO, row, None).await;
    }

    /// Record an attempt that never reached a provider (no usable key).
    async fn record_not_dispatched(&self, mut row: RequestUsage, err: &AiError) {
        tracing::warn!(
            provider = %row.provider,
            request_id = %row.request_id,
            error = %err,
            "no usable API key for provider"
        );
        row.finished_at = Utc::now();
        row.accounting.status = CostStatus::NotCharged;
        row.accounting.charged_cost = Some(Decimal::ZERO);
        row.accounting.error_kind = Some(err.kind());
        self.core.finish(Decimal::ZERO, row, None).await;
    }

    fn fail_logical(&self, logical_id: RequestId, err: &AiError) {
        self.events.emit(AiEvent::RequestFailed(RequestFailed {
            request_id: logical_id,
            provider: None,
            message: err.to_string(),
            at: Utc::now(),
        }));
        self.telemetry.request_failed(&logical_id, &err.to_string());
    }

    /// The single execution path for model requests (chat and stream). Every
    /// physical attempt — first try, retry, fallback — goes through
    /// [`AiClient::execute_attempt`], which owns reservation and settlement.
    async fn execute(
        &self,
        request: ChatRequest,
        opts: RequestOptions,
        mode: Mode,
    ) -> AiResult<Delivery> {
        validate_content(&request)?;
        let logical_id = opts.logical_id.unwrap_or_default();
        let span = tracing::info_span!(
            "ai.request",
            logical_request_id = %logical_id,
            model = %request.model,
            stream = mode == Mode::Stream,
        );
        self.execute_logical(logical_id, request, opts, mode)
            .instrument(span)
            .await
    }

    async fn execute_logical(
        &self,
        logical_id: RequestId,
        request: ChatRequest,
        opts: RequestOptions,
        mode: Mode,
    ) -> AiResult<Delivery> {
        let controlled = self.budget_controlled(&opts.budget);
        self.events.emit(AiEvent::RequestStarted(RequestStarted {
            request_id: logical_id,
            provider: ProviderId::new("pending"),
            model: request.model.clone(),
            at: Utc::now(),
        }));
        self.telemetry
            .request_started(&logical_id, &ProviderId::new("pending"), &request.model);

        let candidates = match self.candidates(&request, mode).await {
            Ok(c) => c,
            Err(err) => {
                self.fail_logical(logical_id, &err);
                return Err(err);
            }
        };
        let allow_fallback = self.config.fallback && !request.side_effecting;
        let request_json = if self.config.store_request_content {
            serde_json::to_value(&request).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            content_redacted_request(&request)
        };
        let policy = &self.config.retry_policy;
        let (mut seq, mut retries, mut fallbacks) = (0u32, 0u32, 0u32);
        let mut charged = Decimal::ZERO;
        let mut last_err = None;

        'providers: for (index, provider) in candidates.iter().enumerate() {
            if index > 0 {
                fallbacks += 1;
            }
            let provider_id = provider.id();
            let binding = match self.bind_provider(provider).await {
                Ok(binding) => binding,
                Err(err) => {
                    seq += 1;
                    let ctx = AttemptCtx::new(logical_id, seq, 0, controlled);
                    let row = self.attempt_row(
                        &ctx,
                        &provider_id,
                        None,
                        &request,
                        &request_json,
                        &opts.budget,
                    );
                    self.record_not_dispatched(row, &err).await;
                    let fallback = allow_fallback && err.is_fallbackable();
                    last_err = Some(err);
                    if fallback {
                        continue 'providers;
                    }
                    break 'providers;
                }
            };
            if let Some(ref kid) = binding.key_id {
                self.keys.touch(kid).await;
            }

            let mut retry = 0u32;
            loop {
                seq += 1;
                let ctx = AttemptCtx::new(logical_id, seq, retry, controlled).after(charged);
                let attempt_id = ctx.attempt_id;
                let result = self
                    .execute_attempt(
                        &binding,
                        &provider_id,
                        &request,
                        &request_json,
                        &opts,
                        &ctx,
                        mode,
                    )
                    .await;
                let err = match result {
                    Ok(delivery) => {
                        tracing::info!(
                            target: "universal_ai::accounting",
                            logical_request_id = %logical_id,
                            attempts = seq,
                            retries,
                            fallbacks,
                            provider = %provider_id,
                            "logical request succeeded"
                        );
                        return Ok(delivery);
                    }
                    Err(AttemptError::Stop(err)) => {
                        self.finish_logical_error(logical_id, seq, retries, fallbacks, &err);
                        return Err(err);
                    }
                    Err(AttemptError::Failed(err)) => err,
                };
                // The failed attempt is settled; its charge counts toward the caps of
                // any further attempt.
                charged += self
                    .usage
                    .get(&attempt_id)
                    .map(|r| r.budget_charge())
                    .unwrap_or_default();
                if policy.should_retry(retry + 1, &err) {
                    let delay = policy.delay_for_attempt(retry, &err);
                    tracing::warn!(
                        provider = %provider_id,
                        retry = retry + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "retrying on the same provider (new physical attempt)"
                    );
                    if let Some(cancel) = opts.cancel.clone() {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancel => {
                                let err = AiError::Cancelled;
                                self.finish_logical_error(logical_id, seq, retries, fallbacks, &err);
                                return Err(err);
                            }
                        }
                    } else {
                        tokio::time::sleep(delay).await;
                    }
                    retry += 1;
                    retries += 1;
                    continue;
                }
                let fallback = allow_fallback && err.is_fallbackable();
                last_err = Some(err);
                if fallback {
                    continue 'providers;
                }
                break 'providers;
            }
        }

        let err = last_err.unwrap_or_else(|| AiError::NoAvailableProvider {
            message: "all providers failed".into(),
        });
        self.finish_logical_error(logical_id, seq, retries, fallbacks, &err);
        Err(err)
    }

    fn finish_logical_error(
        &self,
        logical_id: RequestId,
        attempts: u32,
        retries: u32,
        fallbacks: u32,
        err: &AiError,
    ) {
        tracing::warn!(
            target: "universal_ai::accounting",
            logical_request_id = %logical_id,
            attempts,
            retries,
            fallbacks,
            error_kind = ?err.kind(),
            error = %err,
            "logical request failed"
        );
        self.fail_logical(logical_id, err);
    }

    /// One physical attempt: budget gate → dispatch → settlement. The only place
    /// that calls a provider adapter's `chat` / `stream_chat`.
    #[allow(clippy::too_many_arguments)]
    async fn execute_attempt(
        &self,
        binding: &ProviderBinding,
        provider_id: &ProviderId,
        request: &ChatRequest,
        request_json: &serde_json::Value,
        opts: &RequestOptions,
        ctx: &AttemptCtx,
        mode: Mode,
    ) -> Result<Delivery, AttemptError> {
        let started = Instant::now();
        let mut req = request.clone();
        let mut row = self.attempt_row(
            ctx,
            provider_id,
            Some(binding),
            &req,
            request_json,
            &opts.budget,
        );
        let (reserved, output_bound) = match self
            .preflight(provider_id, &mut req, &opts.budget, ctx, &mut row)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                self.record_rejection(row, &err).await;
                return Err(AttemptError::Stop(err));
            }
        };
        let mut meter = AttemptMeter::new(
            Arc::clone(&self.core),
            row,
            reserved,
            ctx.controlled,
            started,
            (input_bound(&req).total(), output_bound),
        );
        let span = tracing::info_span!(
            "ai.attempt",
            attempt_id = %ctx.attempt_id,
            attempt = ctx.seq,
            retry = ctx.retry,
            provider = %provider_id,
            key_id = ?binding.key_id.as_ref().map(|k| k.to_string()),
        );
        let timeout = opts.timeout.unwrap_or(self.config.default_timeout);
        let reject_missing =
            ctx.controlled && self.config.budget.missing_usage == MissingUsagePolicy::Reject;
        async move {
            meter.dispatched();
            match mode {
                Mode::Chat => {
                    match guarded(binding.provider.chat(req), timeout, opts.cancel.clone()).await {
                        Guarded::Done(Ok(mut response)) => {
                            let status = meter.settle_response(&mut response).await;
                            if reject_missing && status == CostStatus::UsageUnavailable {
                                return Err(AttemptError::Stop(AiError::UsageUnavailable {
                                    provider: provider_id.clone(),
                                    model: response.model,
                                }));
                            }
                            Ok(Delivery::Response(Box::new(response)))
                        }
                        Guarded::Done(Err(err)) => {
                            meter.settle_error(&err).await;
                            Err(AttemptError::Failed(err))
                        }
                        Guarded::TimedOut => {
                            let err = AiError::Timeout;
                            meter.settle_error(&err).await;
                            Err(AttemptError::Failed(err))
                        }
                        Guarded::Cancelled => {
                            meter.settle_abandoned().await;
                            Err(AttemptError::Stop(AiError::Cancelled))
                        }
                    }
                }
                Mode::Stream => {
                    match guarded(
                        binding.provider.stream_chat(req),
                        timeout,
                        opts.cancel.clone(),
                    )
                    .await
                    {
                        Guarded::Done(Ok(stream)) => Ok(Delivery::Stream(metered_stream(
                            stream,
                            meter,
                            reject_missing,
                        ))),
                        Guarded::Done(Err(err)) => {
                            meter.settle_error(&err).await;
                            Err(AttemptError::Failed(err))
                        }
                        Guarded::TimedOut => {
                            let err = AiError::Timeout;
                            meter.settle_error(&err).await;
                            Err(AttemptError::Failed(err))
                        }
                        Guarded::Cancelled => {
                            meter.settle_abandoned().await;
                            Err(AttemptError::Stop(AiError::Cancelled))
                        }
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    /// Select an API key for `provider` and return an adapter bound to it.
    ///
    /// Managed keys for the provider always win; a provider without managed keys
    /// keeps its own constructor credential. Never substitutes a different key: if
    /// the chosen key vanished concurrently (deleted), selection is repeated; a key
    /// whose secret is missing is an error.
    async fn bind_provider(&self, provider: &DynProvider) -> AiResult<ProviderBinding> {
        let provider_id = provider.id();
        for _ in 0..MAX_KEY_SELECTION_ATTEMPTS {
            let Some(info) = self.keys.select_key(&provider_id).await? else {
                return Ok(ProviderBinding {
                    provider: Arc::clone(provider),
                    key_id: None,
                    account_id: self.default_account.clone(),
                });
            };
            match self.keys.get_secret(&info.id).await? {
                Some(secret) => {
                    let credential =
                        ProviderCredential::for_key(info.id.clone(), secret, info.base_url.clone());
                    return Ok(ProviderBinding {
                        provider: provider.with_credential(&credential)?,
                        key_id: Some(info.id),
                        account_id: info.account_id,
                    });
                }
                // Removed between selection and secret lookup: pick again.
                None if self.keys.get_key(&info.id).await.is_none() => continue,
                None => {
                    return Err(AiError::SecretStore {
                        message: format!("secret missing for key {}", info.id),
                    })
                }
            }
        }
        Err(AiError::NoAvailableProvider {
            message: format!("API keys for {provider_id} changed during selection"),
        })
    }

    /// Provider used to estimate `request` (preferred provider, else routing).
    fn estimate_provider(&self, request: &ChatRequest) -> AiResult<ProviderId> {
        match &request.preferred_provider {
            Some(p) => Ok(p.clone()),
            None => self.router().resolve_provider(request.model.as_str()),
        }
    }
}

/// Chat or stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Chat,
    Stream,
}

/// Result of a successful logical request.
enum Delivery {
    Response(Box<ChatResponse>),
    Stream(ChatStream),
}

/// Why an attempt did not deliver.
enum AttemptError {
    /// Ends the logical request (budget / pricing gate, cancellation, usage policy).
    Stop(AiError),
    /// The provider call failed: retry / fallback policy decides what follows.
    Failed(AiError),
}

/// Identity of one physical attempt.
struct AttemptCtx {
    logical_id: RequestId,
    attempt_id: RequestId,
    seq: u32,
    retry: u32,
    controlled: bool,
    /// Charged by earlier attempts of the same logical request: per-request caps
    /// apply to the logical request, not to each attempt separately.
    prior_charged: Decimal,
}

impl AttemptCtx {
    /// The first attempt reuses the logical id (so single-attempt requests keep one id).
    fn new(logical_id: RequestId, seq: u32, retry: u32, controlled: bool) -> Self {
        Self {
            logical_id,
            attempt_id: if seq == 1 {
                logical_id
            } else {
                RequestId::new()
            },
            seq,
            retry,
            controlled,
            prior_charged: Decimal::ZERO,
        }
    }

    fn after(mut self, prior_charged: Decimal) -> Self {
        self.prior_charged = prior_charged;
        self
    }
}

/// Cancellation signal shared by every attempt of a logical request.
type CancelSignal = Shared<BoxFuture<'static, ()>>;

/// Per-request options from [`ChatBuilder`].
#[derive(Default)]
struct RequestOptions {
    budget: RequestBudget,
    /// Per physical attempt; for streams it bounds establishing the stream.
    timeout: Option<Duration>,
    cancel: Option<CancelSignal>,
    /// Caller-supplied logical request id.
    logical_id: Option<RequestId>,
}

enum Guarded<T> {
    Done(T),
    TimedOut,
    Cancelled,
}

/// Run a provider call under the attempt deadline and the caller's cancel signal.
/// Losing the race drops the call future, which aborts the HTTP request.
async fn guarded<F: std::future::Future>(
    fut: F,
    timeout: Duration,
    cancel: Option<CancelSignal>,
) -> Guarded<F::Output> {
    let timed = tokio::time::timeout(timeout, fut);
    match cancel {
        None => match timed.await {
            Ok(v) => Guarded::Done(v),
            Err(_) => Guarded::TimedOut,
        },
        Some(cancel) => tokio::select! {
            r = timed => match r {
                Ok(v) => Guarded::Done(v),
                Err(_) => Guarded::TimedOut,
            },
            _ = cancel => Guarded::Cancelled,
        },
    }
}

/// Reject content the adapters cannot send. Every adapter serializes text only;
/// silently dropping an image or audio part would send (and bill) a different
/// request than the caller built.
fn validate_content(request: &ChatRequest) -> AiResult<()> {
    if request.messages.is_empty() {
        return Err(AiError::InvalidRequest {
            message: "at least one message is required".into(),
        });
    }
    for message in &request.messages {
        let Content::Parts(parts) = &message.content else {
            continue;
        };
        for part in parts {
            match part {
                ContentPart::Text { .. } => {}
                ContentPart::ImageUrl { .. } => {
                    return Err(AiError::UnsupportedCapability {
                        capability: Capability::Images,
                        provider: None,
                    })
                }
                ContentPart::AudioUrl { .. } => {
                    return Err(AiError::UnsupportedCapability {
                        capability: Capability::Audio,
                        provider: None,
                    })
                }
                ContentPart::File { .. } => {
                    return Err(AiError::InvalidRequest {
                        message: "file content parts are not supported by the provider adapters"
                            .into(),
                    })
                }
            }
        }
    }
    Ok(())
}

/// Request metadata without message content (when content storage is disabled).
fn content_redacted_request(request: &ChatRequest) -> serde_json::Value {
    serde_json::json!({
        "model": request.model,
        "messages": request.messages.len(),
        "tools": request.tools.iter().map(|t| t.function.name.clone()).collect::<Vec<_>>(),
        "max_tokens": request.max_tokens,
        "content": "not stored",
    })
}

/// Re-selection attempts when a chosen key is deleted concurrently.
const MAX_KEY_SELECTION_ATTEMPTS: usize = 3;

/// Provider adapter bound to the key selected for one attempt.
struct ProviderBinding {
    provider: DynProvider,
    key_id: Option<KeyId>,
    account_id: AccountId,
}

/// Fluent chat builder.
pub struct ChatBuilder<'a> {
    client: &'a AiClient,
    model: Option<ModelId>,
    messages: Vec<Message>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    side_effecting: bool,
    preferred_provider: Option<ProviderId>,
    tools: Vec<Tool>,
    options: RequestOptions,
}

impl<'a> ChatBuilder<'a> {
    /// Set model.
    pub fn model(mut self, model: impl Into<ModelId>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Prefer a specific provider when resolving (tried first when eligible).
    pub fn provider(mut self, provider: impl Into<ProviderId>) -> Self {
        self.preferred_provider = Some(provider.into());
        self
    }

    /// Append a user message (string).
    pub fn message(mut self, text: impl Into<String>) -> Self {
        self.messages.push(Message::user(text.into()));
        self
    }

    /// Append a full message.
    pub fn add_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Append several messages (e.g. a conversation with tool results).
    pub fn messages(mut self, messages: impl IntoIterator<Item = Message>) -> Self {
        self.messages.extend(messages);
        self
    }

    /// Offer a tool the model may call.
    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Offer several tools the model may call.
    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Temperature.
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Max output tokens (reasoning included). Also the output bound of the
    /// worst-case estimate.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Cap the cost of this logical request (e.g. the remaining run budget): each
    /// attempt is admitted only if its worst case plus what earlier attempts of the
    /// same request (retries / fallbacks) were charged fits. Turns the request
    /// budget-controlled: it is rejected before sending unless its pricing and
    /// output bound are known.
    pub fn max_cost(mut self, amount: Decimal) -> Self {
        self.options.budget.max_cost = Some(amount);
        self
    }

    /// Make this request budget-controlled even when no limit applies: it is sent
    /// only if its worst case can be priced and bounded (otherwise
    /// [`AiError::PricingUnavailable`], [`AiError::OutputLimitUnknown`] or
    /// [`AiError::WorstCaseUnbounded`] before any HTTP), and an attempt whose
    /// actual cost cannot be determined is charged its reservation — never nothing.
    pub fn require_cost_bound(mut self) -> Self {
        self.options.budget.require_cost_bound = true;
        self
    }

    /// Count this request's spend toward `scope` (in addition to the global budget),
    /// optionally enforcing a daily limit for that scope.
    pub fn budget_scope(mut self, scope: impl Into<String>, daily_limit: Option<Decimal>) -> Self {
        self.options.budget.scope = Some(BudgetScope {
            name: scope.into(),
            daily_limit,
        });
        self
    }

    /// Deadline for each physical attempt (default: [`AiConfig::default_timeout`]).
    /// For streams it bounds establishing the stream. A timed-out attempt may have
    /// been billed, so its reservation stays charged.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = Some(timeout);
        self
    }

    /// Cancel the request when `signal` completes: the in-flight HTTP call is
    /// dropped, the attempt is settled as [`CostStatus::Abandoned`] (reservation
    /// kept unless nothing was dispatched) and the call returns
    /// [`AiError::Cancelled`]. Dropping the request future has the same financial
    /// effect. For streams, drop the stream to cancel it.
    pub fn cancel_on(
        mut self,
        signal: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Self {
        self.options.cancel = Some(signal.boxed().shared());
        self
    }

    /// Use `id` as the logical request id (correlation id). The first physical
    /// attempt uses it as its own id; [`AiClient::logical_request_attempts`] finds
    /// every attempt — also when the request failed. Must be unique.
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.options.logical_id = Some(id);
        self
    }

    /// Mark as side-effecting (disables auto-fallback).
    pub fn side_effecting(mut self, yes: bool) -> Self {
        self.side_effecting = yes;
        self
    }

    fn into_request(self) -> AiResult<(&'a AiClient, ChatRequest, RequestOptions)> {
        let model = self.model.ok_or_else(|| AiError::InvalidRequest {
            message: "model is required".into(),
        })?;
        if self.messages.is_empty() {
            return Err(AiError::InvalidRequest {
                message: "at least one message is required".into(),
            });
        }
        let request = ChatRequest {
            model,
            messages: self.messages,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            top_p: None,
            stop: Vec::new(),
            tools: self.tools,
            response_format: None,
            metadata: Default::default(),
            side_effecting: self.side_effecting,
            preferred_provider: self.preferred_provider,
        };
        Ok((self.client, request, self.options))
    }

    /// Send non-streaming request.
    pub async fn send(self) -> AiResult<ChatResponse> {
        let (client, request, options) = self.into_request()?;
        match client.execute(request, options, Mode::Chat).await? {
            Delivery::Response(r) => Ok(*r),
            Delivery::Stream(_) => unreachable!("chat mode delivers a response"),
        }
    }

    /// Start streaming. The stream is metered: its attempt is settled when it ends,
    /// fails, or is dropped (see [`crate::execution`]).
    pub async fn stream(self) -> AiResult<ChatStream> {
        let (client, request, options) = self.into_request()?;
        match client.execute(request, options, Mode::Stream).await? {
            Delivery::Stream(s) => Ok(s),
            Delivery::Response(_) => unreachable!("stream mode delivers a stream"),
        }
    }
}

/// Cost estimation surface.
pub struct CostApi<'a> {
    client: &'a AiClient,
}

impl CostApi<'_> {
    /// Upper-bound estimate for a chat request, computed exactly like the budget
    /// gate (input byte bound, output bound, highest applicable rates). Without an
    /// output bound (`max_tokens` unset, model limit unknown) the result covers
    /// input only (`output_is_upper_bound == false`). `None` when the model has no
    /// price. Use [`CostEstimate::explain`] for the derivation.
    pub async fn estimate(&self, request: &ChatRequest) -> AiResult<Option<CostEstimate>> {
        let c = self.client;
        let provider = c.estimate_provider(request)?;
        let output = output_bound(&c.models, &provider, request).ok();
        Ok(c.cost
            .estimate_bounds(&provider, &request.model, input_bound(request), output))
    }

    /// The worst case the budget gate would reserve for `request`, or the
    /// fail-closed error it would reject it with.
    pub async fn worst_case(&self, request: &ChatRequest) -> AiResult<CostEstimate> {
        let c = self.client;
        let provider = c.estimate_provider(request)?;
        worst_case_cost(&c.cost, &c.models, &provider, request)
    }
}

/// Stats surface.
pub struct StatsApi<'a> {
    client: &'a AiClient,
}

impl StatsApi<'_> {
    /// All-time in-memory stats.
    pub async fn all(&self) -> UsageStatistics {
        self.client.usage.statistics()
    }

    /// Today (UTC).
    pub async fn today(&self) -> UsageStatistics {
        self.client
            .usage
            .statistics_for_day(Utc::now().date_naive())
    }
}

/// Events surface.
pub struct EventApi<'a> {
    bus: &'a EventBus,
}

impl EventApi<'_> {
    /// Subscribe to the event bus.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AiEvent> {
        self.bus.subscribe()
    }
}

/// Monitor surface.
pub struct MonitorApi<'a> {
    client: &'a AiClient,
}

impl MonitorApi<'_> {
    /// Balance monitor.
    pub fn balances(&self) -> BalanceMonitor {
        BalanceMonitor::new(
            self.client.provider_snapshot(),
            Arc::clone(&self.client.balances),
            BalanceMonitorConfig::default(),
        )
    }
}

// Re-export helpers used in docs.
pub use crate::router::TaskType as RouterTaskType;
