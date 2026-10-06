//! AiClient — public unified API.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, Utc};

use crate::account::{AccountManager, ApiKeyInfo, ApiKeyManager};
use crate::balance::{Balance, BalanceManager, BalanceMonitor, BalanceMonitorConfig};
use crate::capability::Capability;
use crate::config::{AiConfig, BudgetPolicy};
use crate::cost::{CostEstimate, CostManager};
use crate::error::{AiError, AiResult};
use crate::events::{AiEvent, EventBus, RequestCompleted, RequestFailed, RequestStarted};
use crate::health::HealthMonitor;
use crate::http::{HttpClient, HttpConfig};
use crate::models::{ModelRegistry, ModelsApi};
use crate::pricing::PricingRegistry;
use crate::provider::{DynProvider, Provider};
use crate::provider_catalog::build_provider;
use crate::retry::with_retry;
use crate::router::{MaxCost, Router, TaskType};
use crate::secrets::{MemorySecretStore, SecretStore};
use crate::storage::{MemoryStorage, Storage};
use crate::telemetry::{NoopTelemetry, TelemetrySink};
use crate::types::{
    AccountId, ChatRequest, ChatResponse, ChatStream, KeyId, Message, ModelId, ProviderId,
    RequestId,
};
use crate::usage::{RequestUsage, Usage, UsageManager, UsageStatistics};

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
    telemetry: Arc<dyn TelemetrySink>,
    default_account: AccountId,
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
        for p in &self.providers {
            let _ = p.id();
        }

        Ok(AiClient {
            providers: RwLock::new(self.providers),
            http,
            config: self.config,
            events: Arc::clone(&events),
            accounts: AccountManager::new(),
            keys: Arc::new(ApiKeyManager::new(store)),
            balances: Arc::new(BalanceManager::new()),
            usage: Arc::new(UsageManager::new()),
            cost: Arc::new(CostManager::new(pricing)),
            models,
            health: Arc::new(HealthMonitor::new(Some(events))),
            storage,
            telemetry,
            default_account: AccountId::new("default"),
        })
    }
}

impl AiClient {
    /// Builder entry.
    pub fn builder() -> AiClientBuilder {
        AiClientBuilder::new()
    }

    /// Fluent chat API.
    pub fn chat(&self) -> ChatBuilder<'_> {
        ChatBuilder {
            client: self,
            model: None,
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            side_effecting: false,
            preferred_provider: None,
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
        for p in self.providers() {
            if !p.supports(Capability::ModelList) {
                continue;
            }
            match p.list_models().await {
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
            .providers()
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
        let bal = p.balance().await?;
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

        for provider in self.providers() {
            let provider_id = provider.id();
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
                match provider
                    .usage(UsageRequest { start, end })
                    .await
                {
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
        EventApi {
            bus: &self.events,
        }
    }

    /// List persisted AI request rows (newest first).
    pub async fn list_ai_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>> {
        self.storage.list_requests(limit).await
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

    /// Configured providers (snapshot).
    pub fn providers(&self) -> Vec<DynProvider> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

    /// Build and register a provider from a managed API key (+ optional base URL).
    pub async fn sync_provider_from_key(
        &self,
        info: &ApiKeyInfo,
        base_url: Option<&str>,
    ) -> AiResult<()> {
        let secret = self
            .keys
            .get_secret(&info.id)
            .await?
            .ok_or_else(|| AiError::InvalidRequest {
                message: format!("secret missing for key {}", info.id),
            })?;
        let provider = build_provider(&info.provider, secret, base_url, self.http.clone())?;
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

    /// Router builder.
    pub fn router(&self) -> Router {
        Router::new(
            self.providers(),
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
        let providers = self.providers();
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

    async fn execute_chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let request_id = RequestId::new();
        let started_at = Utc::now();
        let started = Instant::now();

        // Budget estimate (best-effort).
        if let Some(provider_id) = request
            .preferred_provider
            .clone()
            .or_else(|| self.router().resolve_provider(request.model.as_str()).ok())
        {
            if let Ok(Some(estimate)) = self.cost.estimate(
                &provider_id,
                &request.model,
                estimate_input_tokens(&request),
                request.max_tokens.map(|t| t as u64),
            ) {
                let stats = self.usage.statistics_for_day(Utc::now().date_naive());
                let today = Utc::now().date_naive();
                let month_start = today.with_day(1).unwrap_or(today);
                let month_stats = {
                    let mut s = UsageStatistics::default();
                    for row in self.usage.list() {
                        if row.started_at.date_naive() >= month_start {
                            s.record(&row);
                        }
                    }
                    s
                };
                self.cost.check_budget(
                    &estimate,
                    self.config.budget.max_request_cost,
                    stats.total_cost,
                    self.config.budget.max_daily_cost,
                    month_stats.total_cost,
                    self.config.budget.max_monthly_cost,
                )?;
            }
        }

        self.events.emit(AiEvent::RequestStarted(RequestStarted {
            request_id,
            provider: ProviderId::new("pending"),
            model: request.model.clone(),
            at: started_at,
        }));
        self.telemetry
            .request_started(&request_id, &ProviderId::new("pending"), &request.model);

        let providers = self.resolve_providers_for_model(&request)?;
        let allow_fallback = self.config.fallback && !request.side_effecting;
        let mut last_err = None;
        let mut last_attempt: Option<(ProviderId, AccountId, Option<KeyId>, ModelId)> = None;
        let request_json = serde_json::to_value(&request).unwrap_or_else(|_| serde_json::json!({}));

        for provider in providers {
            if !self.health.is_healthy(&provider.id()).await {
                continue;
            }
            if !provider.supports(Capability::Chat) {
                continue;
            }

            let provider_id = provider.id();
            let selected_key = self.keys.select_key(&provider_id).await?.map(|k| k.id);
            let account_id = if let Some(ref kid) = selected_key {
                self.keys
                    .get_key(kid)
                    .await
                    .map(|k| k.account_id)
                    .unwrap_or_else(|| self.default_account.clone())
            } else {
                self.default_account.clone()
            };
            if let Some(ref kid) = selected_key {
                self.keys.touch(kid).await;
            }

            let model = request.model.clone();
            last_attempt = Some((
                provider_id.clone(),
                account_id.clone(),
                selected_key.clone(),
                model.clone(),
            ));
            let req = request.clone();
            let policy = self.config.retry_policy.clone();

            let result = with_retry(&policy, || {
                let provider = Arc::clone(&provider);
                let req = req.clone();
                async move { provider.chat(req).await }
            })
            .await;

            match result {
                Ok(mut response) => {
                    response.request_id = request_id;
                    if let Some(usage) = &response.usage {
                        if let Ok(Some(cost)) =
                            self.cost.calculate(&provider_id, &response.model, usage)
                        {
                            response.cost = Some(cost);
                        }
                    }
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let finished_at = Utc::now();
                    let response_json =
                        serde_json::to_value(&response).unwrap_or_else(|_| serde_json::json!({}));
                    let row = RequestUsage {
                        request_id,
                        provider: provider_id.clone(),
                        account: account_id,
                        api_key: selected_key.clone(),
                        model: response.model.clone(),
                        started_at,
                        finished_at,
                        usage: response.usage.clone().unwrap_or_default(),
                        cost: response.cost.clone(),
                        success: true,
                        latency_ms,
                        request_json: request_json.clone(),
                        response_json: Some(response_json),
                        importance: None,
                    };
                    self.usage.record(row.clone());
                    let _ = self.storage.save_request(&row).await;

                    tracing::info!(
                        provider = %provider_id,
                        model = %model,
                        request_id = %request_id,
                        latency_ms,
                        "AI request completed"
                    );

                    self.events.emit(AiEvent::RequestCompleted(RequestCompleted {
                        request_id,
                        provider: provider_id.clone(),
                        model: model.clone(),
                        usage: response.usage.clone(),
                        latency_ms,
                        at: finished_at,
                    }));
                    self.telemetry.request_completed(
                        &request_id,
                        &provider_id,
                        &model,
                        response.usage.as_ref(),
                        latency_ms,
                    );
                    return Ok(response);
                }
                Err(err) => {
                    tracing::warn!(
                        provider = %provider_id,
                        model = %model,
                        request_id = %request_id,
                        error = %err,
                        "AI request failed"
                    );
                    last_err = Some(err);
                    if !allow_fallback {
                        break;
                    }
                }
            }
        }

        let err = last_err.unwrap_or_else(|| AiError::NoAvailableProvider {
            message: "all providers failed".into(),
        });
        let finished_at = Utc::now();
        let latency_ms = started.elapsed().as_millis() as u64;
        let (provider, account, api_key, model) = last_attempt.unwrap_or_else(|| {
            (
                ProviderId::new("none"),
                self.default_account.clone(),
                None,
                request.model.clone(),
            )
        });
        let fail_row = RequestUsage {
            request_id,
            provider,
            account,
            api_key,
            model,
            started_at,
            finished_at,
            usage: Usage::default(),
            cost: None,
            success: false,
            latency_ms,
            request_json,
            response_json: None,
            importance: None,
        };
        self.usage.record(fail_row.clone());
        let _ = self.storage.save_request(&fail_row).await;

        self.events.emit(AiEvent::RequestFailed(RequestFailed {
            request_id,
            provider: None,
            message: err.to_string(),
            at: finished_at,
        }));
        self.telemetry
            .request_failed(&request_id, &err.to_string());
        Err(err)
    }

    async fn execute_stream(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let providers = self.resolve_providers_for_model(&request)?;
        for provider in providers {
            if !provider.supports(Capability::Streaming) {
                continue;
            }
            match provider.stream_chat(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(err) if self.config.fallback && !request.side_effecting => {
                    tracing::warn!(provider = %provider.id(), error = %err, "stream failed, trying fallback");
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(AiError::NoAvailableProvider {
            message: "no streaming provider available".into(),
        })
    }
}

fn estimate_input_tokens(request: &ChatRequest) -> u64 {
    let chars: usize = request
        .messages
        .iter()
        .map(|m| m.content.to_plain_text().len())
        .sum();
    // Rough 4 chars/token heuristic for pre-flight budget only.
    (chars as u64 / 4).max(1)
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
}

impl ChatBuilder<'_> {
    /// Set model.
    pub fn model(mut self, model: impl Into<ModelId>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Prefer a specific provider when resolving.
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

    /// Temperature.
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Max tokens.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Mark as side-effecting (disables auto-fallback).
    pub fn side_effecting(mut self, yes: bool) -> Self {
        self.side_effecting = yes;
        self
    }

    fn into_request(self) -> AiResult<ChatRequest> {
        let model = self.model.ok_or_else(|| AiError::InvalidRequest {
            message: "model is required".into(),
        })?;
        if self.messages.is_empty() {
            return Err(AiError::InvalidRequest {
                message: "at least one message is required".into(),
            });
        }
        Ok(ChatRequest {
            model,
            messages: self.messages,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            top_p: None,
            stop: Vec::new(),
            tools: Vec::new(),
            response_format: None,
            metadata: Default::default(),
            side_effecting: self.side_effecting,
            preferred_provider: self.preferred_provider,
        })
    }

    /// Send non-streaming request.
    pub async fn send(self) -> AiResult<ChatResponse> {
        let client = self.client;
        let request = self.into_request()?;
        client.execute_chat(request).await
    }

    /// Start streaming.
    pub async fn stream(self) -> AiResult<ChatStream> {
        let client = self.client;
        let request = self.into_request()?;
        client.execute_stream(request).await
    }
}

/// Cost estimation surface.
pub struct CostApi<'a> {
    client: &'a AiClient,
}

impl CostApi<'_> {
    /// Estimate from a chat request.
    pub async fn estimate(&self, request: &ChatRequest) -> AiResult<Option<CostEstimate>> {
        let provider = self.client.router().resolve_provider(request.model.as_str())?;
        self.client.cost.estimate(
            &provider,
            &request.model,
            estimate_input_tokens(request),
            request.max_tokens.map(|t| t as u64),
        )
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
            self.client.providers(),
            Arc::clone(&self.client.balances),
            BalanceMonitorConfig::default(),
        )
    }
}

// Re-export helpers used in docs.
pub use crate::router::TaskType as RouterTaskType;

/// Silence unused import in some feature sets.
#[allow(dead_code)]
fn _use_duration() -> Duration {
    Duration::from_secs(1)
}

#[allow(dead_code)]
fn _use_usage() -> Usage {
    Usage::default()
}

#[allow(dead_code)]
fn _use_max_cost() {
    let _ = MaxCost::usd;
    let _ = TaskType::TextGeneration;
}
