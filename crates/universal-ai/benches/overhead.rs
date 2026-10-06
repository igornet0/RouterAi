//! Overhead benchmarks (no external harness): `cargo bench -p universal-ai --bench overhead`.
//!
//! A zero-latency in-process provider isolates library overhead from network
//! time. Reported numbers are means over many iterations on the current machine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use rust_decimal::Decimal;
use universal_ai::{
    Account, AiClient, AiConfig, AiResult, Balance, BudgetPolicy, ChatRequest, ChatResponse,
    ChatStream, CostStatus, HealthStatus, Message, ModelPricing, Provider, ProviderCapabilities,
    ProviderId, RequestId, RequestUsage, RetryPolicy, SqliteStorage, Storage, StreamEvent, Usage,
};

#[derive(Clone)]
struct Instant0;

#[async_trait]
impl Provider for Instant0 {
    fn id(&self) -> ProviderId {
        ProviderId::new("bench")
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: true,
            ..Default::default()
        }
    }
    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        Ok(ChatResponse {
            request_id: RequestId::new(),
            model: request.model,
            message: Message::assistant("ok"),
            finish_reason: None,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            cost: None,
            raw: None,
        })
    }
    async fn stream_chat(&self, _request: ChatRequest) -> AiResult<ChatStream> {
        let mut events: Vec<AiResult<StreamEvent>> = (0..100)
            .map(|_| Ok(StreamEvent::TextDelta { text: "x".into() }))
            .collect();
        events.push(Ok(StreamEvent::Usage {
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 100,
                total_tokens: 110,
                ..Default::default()
            },
        }));
        events.push(Ok(StreamEvent::Done));
        Ok(Box::pin(futures::stream::iter(events)))
    }
    async fn health(&self) -> AiResult<HealthStatus> {
        Ok(HealthStatus::ok(0))
    }
}

/// Records how long reservation (Pending) and settlement writes take.
struct Timed<S> {
    inner: S,
    reserve_ns: AtomicU64,
    reserves: AtomicU64,
    settle_ns: AtomicU64,
    settles: AtomicU64,
}

impl<S> Timed<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            reserve_ns: AtomicU64::new(0),
            reserves: AtomicU64::new(0),
            settle_ns: AtomicU64::new(0),
            settles: AtomicU64::new(0),
        }
    }
    fn means(&self) -> (Duration, Duration) {
        let mean = |ns: &AtomicU64, n: &AtomicU64| {
            Duration::from_nanos(ns.load(Ordering::Relaxed) / n.load(Ordering::Relaxed).max(1))
        };
        (
            mean(&self.reserve_ns, &self.reserves),
            mean(&self.settle_ns, &self.settles),
        )
    }
}

#[async_trait]
impl<S: Storage> Storage for Timed<S> {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        let t = Instant::now();
        let r = self.inner.save_request(row).await;
        let ns = t.elapsed().as_nanos() as u64;
        if row.accounting.status == CostStatus::Pending {
            self.reserve_ns.fetch_add(ns, Ordering::Relaxed);
            self.reserves.fetch_add(1, Ordering::Relaxed);
        } else {
            self.settle_ns.fetch_add(ns, Ordering::Relaxed);
            self.settles.fetch_add(1, Ordering::Relaxed);
        }
        r
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

fn client(budget: BudgetPolicy, storage: Option<Arc<dyn Storage>>) -> AiClient {
    let mut b = AiClient::builder().provider(Instant0).config(AiConfig {
        budget,
        retry_policy: RetryPolicy {
            max_attempts: 1,
            ..Default::default()
        },
        ..AiConfig::default()
    });
    if let Some(s) = storage {
        b = b.storage(s);
    }
    let c = b.build().unwrap();
    c.pricing().upsert(ModelPricing::per_million(
        ProviderId::new("bench"),
        "m",
        Decimal::ONE,
        Decimal::TWO,
    ));
    c
}

async fn send(c: &AiClient) {
    c.chat()
        .model("m")
        .message("hello world")
        .max_tokens(100)
        .send()
        .await
        .unwrap();
}

fn report(name: &str, total: Duration, n: u64) {
    println!(
        "{name:<48} {:>10.1} µs/op  ({n} ops)",
        total.as_secs_f64() * 1e6 / n as f64
    );
}

async fn time<F, Fut>(n: u64, mut f: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let t = Instant::now();
    for _ in 0..n {
        f().await;
    }
    t.elapsed()
}

fn temp_db() -> (String, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("uai-bench-{}.db", RequestId::new()));
    (format!("sqlite://{}?mode=rwc", path.display()), path)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    // `cargo test --benches` runs this without `--bench`: keep it short there.
    let full = std::env::args().any(|a| a == "--bench");
    let n: u64 = if full { 5_000 } else { 50 };
    let budget = BudgetPolicy::daily_usd(Decimal::from(1_000_000));

    // 1. Adapter call alone vs the full execution path.
    let raw = Instant0;
    let req = ChatRequest::simple("m", "hello world");
    let t = time(n, || async {
        raw.chat(req.clone()).await.map(|_| ()).unwrap()
    })
    .await;
    report("adapter call (no library path)", t, n);
    let c = client(BudgetPolicy::default(), None);
    let t = time(n, || send(&c)).await;
    report("send(), no budget, memory storage", t, n);
    let c = client(budget.clone(), None);
    let t = time(n, || send(&c)).await;
    report("send(), budget (reserve+settle), memory storage", t, n);

    // 2. SQLite: reservation / settlement latency.
    let (url, path) = temp_db();
    let timed = Arc::new(Timed::new(SqliteStorage::connect(&url).await.unwrap()));
    let c = client(budget.clone(), Some(timed.clone()));
    let t = time(n.min(2_000), || send(&c)).await;
    report("send(), budget, SQLite (2 durable writes)", t, n.min(2_000));
    let (reserve, settle) = timed.means();
    println!(
        "{:<48} {:>10.1} µs",
        "  reservation write (Pending row)",
        reserve.as_secs_f64() * 1e6
    );
    println!(
        "{:<48} {:>10.1} µs",
        "  settlement write (final row)",
        settle.as_secs_f64() * 1e6
    );
    let (wal_url, wal_path) = temp_db();
    let wal = SqliteStorage::connect(&wal_url).await.unwrap();
    wal.enable_wal().await.unwrap();
    let timed_wal = Arc::new(Timed::new(wal));
    let c = client(budget.clone(), Some(timed_wal.clone()));
    let t = time(n.min(2_000), || send(&c)).await;
    report("send(), budget, SQLite WAL (opt-in)", t, n.min(2_000));
    let (reserve, settle) = timed_wal.means();
    println!(
        "{:<48} {:>10.1} µs",
        "  reservation write, WAL",
        reserve.as_secs_f64() * 1e6
    );
    println!(
        "{:<48} {:>10.1} µs",
        "  settlement write, WAL",
        settle.as_secs_f64() * 1e6
    );
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", wal_path.display()));
    }

    // 3. SQLite spend lookup (cold ledger load after restart) over existing rows.
    let storage = SqliteStorage::connect(&url).await.unwrap();
    let rows = storage.list_requests(usize::MAX).await.unwrap().len();
    let start = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let lookups = if full { 200 } else { 5 };
    let t = time(lookups, || async {
        storage
            .spend_in_window(None, start, start + chrono::Duration::days(1))
            .await
            .unwrap();
    })
    .await;
    report(&format!("spend_in_window over {rows} rows"), t, lookups);

    // 4. Stream overhead (100 deltas + usage + done).
    let raw_stream = time(n.min(2_000), || async {
        let mut s = raw.stream_chat(req.clone()).await.unwrap();
        while s.next().await.is_some() {}
    })
    .await;
    report(
        "adapter stream, 102 events (no metering)",
        raw_stream,
        n.min(2_000),
    );
    let c = client(budget.clone(), None);
    let metered = time(n.min(2_000), || async {
        let mut s = c
            .chat()
            .model("m")
            .message("hello world")
            .max_tokens(100)
            .stream()
            .await
            .unwrap();
        while s.next().await.is_some() {}
    })
    .await;
    report("metered stream, 102 events, budget", metered, n.min(2_000));

    // 5. Concurrency: throughput vs in-flight requests (ledger lock + storage).
    // A fresh client (and database) per data point, so rows from earlier points
    // do not slow later ones.
    for label in ["memory", "sqlite", "sqlite-wal"] {
        for concurrency in [1usize, 8, 64, 512] {
            let (url, path) = temp_db();
            let storage: Option<Arc<dyn Storage>> = match label {
                "memory" => None,
                _ => {
                    let s = SqliteStorage::connect(&url).await.unwrap();
                    if label == "sqlite-wal" {
                        s.enable_wal().await.unwrap();
                    }
                    Some(Arc::new(s))
                }
            };
            let c = Arc::new(client(budget.clone(), storage));
            let total = if full { 2_048 } else { 64 };
            let done = Arc::new(Mutex::new(0usize));
            let t = Instant::now();
            let mut tasks = Vec::new();
            for _ in 0..concurrency {
                let c = Arc::clone(&c);
                let done = Arc::clone(&done);
                tasks.push(tokio::spawn(async move {
                    loop {
                        {
                            let mut d = done.lock().unwrap();
                            if *d >= total {
                                break;
                            }
                            *d += 1;
                        }
                        send(&c).await;
                    }
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            let secs = t.elapsed().as_secs_f64();
            println!(
                "{:<48} {:>10.0} req/s",
                format!("throughput, {label}, {concurrency} in flight"),
                total as f64 / secs
            );
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
    let _ = std::fs::remove_file(path);
}
