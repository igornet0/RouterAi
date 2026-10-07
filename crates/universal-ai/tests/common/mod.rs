//! Deterministic in-process provider + storage wrappers for financial tests.
//!
//! Prices: input $1000/M, output $2000/M. `message("hi")` with `max_tokens(100)`
//! has a worst case of **0.218**; usage 10 in / 5 out costs **0.02**.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use universal_ai::{
    Account, AiClient, AiConfig, AiError, AiResult, Balance, BudgetPolicy, ChatRequest,
    ChatResponse, ChatStream, CostStatus, HealthStatus, Message, ModelId, ModelPricing, Provider,
    ProviderCapabilities, ProviderErrorDetails, ProviderId, RequestId, RequestUsage, RetryPolicy,
    Storage, StreamEvent, Usage,
};

pub fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

pub const WORST: &str = "0.218";
pub const ACTUAL: &str = "0.02";

pub fn usage(p: u64, c: u64) -> Usage {
    Usage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
        ..Default::default()
    }
}

/// What one provider call does.
#[derive(Clone)]
pub enum Step {
    /// Respond with this usage (None = provider sent no usage).
    Ok(Option<Usage>),
    /// Respond after a delay.
    Delayed(Duration, Option<Usage>),
    /// Never respond (until the attempt deadline / cancellation drops the call).
    Hang,
    /// Fail with a definitive HTTP status.
    Status(u16),
    /// Fail with a transport error (may have consumed tokens).
    Network,
    /// Malformed response body.
    Malformed,
    /// Stream these items, then end (or hang when `hang_at_end`).
    Stream {
        items: Vec<StreamItem>,
        hang_at_end: bool,
    },
}

#[derive(Clone)]
pub enum StreamItem {
    Text(&'static str),
    Usage(Usage),
    Done,
    NetworkError,
}

type Script = dyn Fn(usize, &ChatRequest) -> Step + Send + Sync;

/// Provider whose behavior is a pure function of (call index, request).
#[derive(Clone)]
pub struct Scripted {
    pub id: ProviderId,
    pub calls: Arc<AtomicUsize>,
    script: Arc<Script>,
    /// Set when the inner stream of a call is dropped (connection "closed").
    pub streams_dropped: Arc<AtomicUsize>,
    /// Called at dispatch time (e.g. to check ledger invariants).
    pub on_call: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Scripted {
    pub fn new(
        id: &str,
        script: impl Fn(usize, &ChatRequest) -> Step + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: ProviderId::new(id),
            calls: Arc::new(AtomicUsize::new(0)),
            script: Arc::new(script),
            streams_dropped: Arc::new(AtomicUsize::new(0)),
            on_call: None,
        }
    }

    pub fn always(id: &str, step: Step) -> Self {
        Self::new(id, move |_, _| step.clone())
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn status_error(id: &ProviderId, status: u16) -> AiError {
    AiError::from_http_status(id.clone(), status, "scripted failure", None)
}

fn response(req: &ChatRequest, usage: Option<Usage>) -> ChatResponse {
    ChatResponse {
        request_id: RequestId::new(),
        model: req.model.clone(),
        message: Message::assistant("ok"),
        finish_reason: None,
        usage,
        cost: None,
        raw: None,
    }
}

struct DropFlag(Arc<AtomicUsize>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Provider for Scripted {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = &self.on_call {
            f();
        }
        match (self.script)(n, &request) {
            Step::Ok(u) => Ok(response(&request, u)),
            Step::Delayed(delay, u) => {
                tokio::time::sleep(delay).await;
                Ok(response(&request, u))
            }
            Step::Hang => futures::future::pending().await,
            Step::Status(s) => Err(status_error(&self.id, s)),
            Step::Network => Err(AiError::network("connection reset", true)),
            Step::Malformed => Err(AiError::Serialization {
                message: "missing choices".into(),
            }),
            Step::Stream { .. } => panic!("stream step used for chat"),
        }
    }

    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = &self.on_call {
            f();
        }
        let (items, hang_at_end) = match (self.script)(n, &request) {
            Step::Stream { items, hang_at_end } => (items, hang_at_end),
            Step::Status(s) => return Err(status_error(&self.id, s)),
            Step::Network => return Err(AiError::network("connection reset", true)),
            Step::Hang => futures::future::pending().await,
            _ => panic!("chat step used for stream"),
        };
        let flag = DropFlag(Arc::clone(&self.streams_dropped));
        let events: Vec<AiResult<StreamEvent>> = items
            .into_iter()
            .map(|i| match i {
                StreamItem::Text(t) => Ok(StreamEvent::TextDelta { text: t.into() }),
                StreamItem::Usage(u) => Ok(StreamEvent::Usage { usage: u }),
                StreamItem::Done => Ok(StreamEvent::Done),
                StreamItem::NetworkError => Err(AiError::network("stream reset", true)),
            })
            .collect();
        let tail: futures::stream::BoxStream<'static, AiResult<StreamEvent>> = if hang_at_end {
            Box::pin(futures::stream::pending())
        } else {
            Box::pin(futures::stream::empty())
        };
        use futures::StreamExt;
        let stream = futures::stream::iter(events).chain(tail).map(move |e| {
            let _keep = &flag;
            e
        });
        Ok(Box::pin(stream))
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        Ok(HealthStatus::ok(0))
    }
}

/// Unused helper kept for symmetry with HTTP errors.
pub fn provider_error(id: &str, status: u16) -> AiError {
    AiError::Provider {
        details: ProviderErrorDetails {
            provider: ProviderId::new(id),
            http_status: Some(status),
            provider_error_code: None,
            message: "x".into(),
            request_id: None,
            retryable: true,
            retry_after_secs: None,
        },
    }
}

pub fn price(provider: &str, model: &str) -> ModelPricing {
    ModelPricing::per_million(ProviderId::new(provider), model, d("1000"), d("2000"))
}

pub struct ClientOpts {
    pub budget: BudgetPolicy,
    pub max_attempts: u32,
    pub fallback: bool,
    pub storage: Option<Arc<dyn Storage>>,
    pub timeout: Duration,
}

impl Default for ClientOpts {
    fn default() -> Self {
        Self {
            budget: BudgetPolicy::daily_usd(d("10")),
            max_attempts: 1,
            fallback: false,
            storage: None,
            timeout: Duration::from_secs(5),
        }
    }
}

pub fn client(providers: Vec<Scripted>, opts: ClientOpts) -> AiClient {
    let mut builder = AiClient::builder().config(AiConfig {
        budget: opts.budget,
        fallback: opts.fallback,
        default_timeout: opts.timeout,
        retry_policy: RetryPolicy {
            max_attempts: opts.max_attempts,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            exponential_backoff: false,
        },
        ..AiConfig::default()
    });
    for p in &providers {
        builder = builder.provider(p.clone());
    }
    if let Some(storage) = opts.storage {
        builder = builder.storage(storage);
    }
    let client = builder.build().unwrap();
    for p in &providers {
        client.pricing().upsert(price(p.id.as_str(), "m"));
    }
    client
}

pub async fn send(client: &AiClient) -> Result<ChatResponse, AiError> {
    client
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .send()
        .await
}

pub async fn spent_today(client: &AiClient) -> Decimal {
    client.budget_status(None).await.unwrap().daily_spent
}

/// Wait (bounded) until `cond` holds — for settlements spawned on drop.
pub async fn eventually<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not reached");
}

/// Financial invariants over every persisted row.
pub async fn assert_invariants(client: &AiClient, calls: usize) {
    let rows = client.list_ai_requests(usize::MAX).await.unwrap();
    let charged: Decimal = rows.iter().map(RequestUsage::budget_charge).sum();
    let status = client.budget_status(None).await.unwrap();
    assert_eq!(
        charged, status.daily_spent,
        "sum(charged) == persisted spend"
    );
    assert_eq!(
        client.stats().all().await.charged_cost,
        charged,
        "statistics agree with the ledger"
    );
    let dispatched = rows.iter().filter(|r| r.accounting.dispatched).count();
    assert_eq!(dispatched, calls, "one row per physical attempt");
    for r in &rows {
        let a = &r.accounting;
        assert!(r.budget_charge() >= Decimal::ZERO);
        assert!(a.reserved_cost.unwrap_or_default() >= Decimal::ZERO);
        assert_ne!(a.status, CostStatus::Pending, "everything settled");
        match a.status {
            CostStatus::Actual => {
                assert_eq!(
                    a.charged_cost,
                    r.cost.as_ref().map(|c| c.amount),
                    "settled == actual"
                )
            }
            CostStatus::Reconciled => {
                assert_eq!(
                    a.charged_cost,
                    r.cost.as_ref().map(|c| c.amount),
                    "reconciled == the reconciled cost"
                )
            }
            CostStatus::Rejected => {
                assert!(!a.dispatched, "rejected => not sent");
                assert_eq!(a.charged_cost, Some(Decimal::ZERO));
            }
            CostStatus::UsageUnavailable | CostStatus::PricingUnavailable => {
                assert!(
                    a.charged_cost.is_some_and(|c| c > Decimal::ZERO) || a.estimated_cost.is_none(),
                    "unknown cost is not zero: {a:?}"
                );
            }
            _ => {}
        }
    }
}

/// Storage wrapper that records the maximum committed spend ever persisted and
/// can check it against a limit at any time.
pub struct CheckingStorage {
    pub inner: universal_ai::MemoryStorage,
    pub max_committed: Mutex<Decimal>,
    pub saves: Mutex<HashMap<RequestId, usize>>,
}

impl CheckingStorage {
    pub fn new() -> Self {
        Self {
            inner: universal_ai::MemoryStorage::new(),
            max_committed: Mutex::new(Decimal::ZERO),
            saves: Mutex::new(HashMap::new()),
        }
    }

    pub fn committed_now(&self) -> Decimal {
        futures::executor::block_on(self.inner.list_requests(usize::MAX))
            .unwrap()
            .iter()
            .map(RequestUsage::budget_charge)
            .sum()
    }

    pub fn max_committed(&self) -> Decimal {
        *self.max_committed.lock().unwrap()
    }

    pub fn saves_of(&self, id: &RequestId) -> usize {
        self.saves.lock().unwrap().get(id).copied().unwrap_or(0)
    }
}

#[async_trait]
impl Storage for CheckingStorage {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        self.inner.save_request(row).await?;
        *self
            .saves
            .lock()
            .unwrap()
            .entry(row.request_id)
            .or_default() += 1;
        let committed = self.committed_now();
        let mut max = self.max_committed.lock().unwrap();
        if committed > *max {
            *max = committed;
        }
        Ok(())
    }
    async fn get_request(&self, id: &RequestId) -> AiResult<Option<RequestUsage>> {
        self.inner.get_request(id).await
    }
    async fn set_importance(&self, id: &RequestId, i: Option<u8>) -> AiResult<RequestUsage> {
        self.inner.set_importance(id, i).await
    }
    async fn save_balance(&self, b: &Balance) -> AiResult<()> {
        self.inner.save_balance(b).await
    }
    async fn save_account(&self, a: &Account) -> AiResult<()> {
        self.inner.save_account(a).await
    }
    async fn list_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>> {
        self.inner.list_requests(limit).await
    }
    async fn list_balances(&self) -> AiResult<Vec<Balance>> {
        self.inner.list_balances().await
    }
    async fn list_accounts(&self) -> AiResult<Vec<Account>> {
        self.inner.list_accounts().await
    }
    async fn spend_in_window(
        &self,
        scope: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> AiResult<Decimal> {
        self.inner.spend_in_window(scope, start, end).await
    }
}

/// Deterministic xorshift PRNG (no external crates).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn model() -> ModelId {
    ModelId::new("m")
}
