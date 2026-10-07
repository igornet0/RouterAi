//! Settlement must survive cancellation of the caller.
//!
//! Dropping a request future (or a stream) *while its settlement is being
//! persisted* — e.g. waiting for the ledger lock or a slow SQLite write under
//! load — must not leave the attempt `Pending` for the rest of the process or
//! drop it from the statistics.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::*;
use futures::StreamExt;
use rust_decimal::Decimal;
use tokio::sync::{Notify, Semaphore};
use universal_ai::{
    Account, AiResult, Balance, CostStatus, MemoryStorage, RequestId, RequestUsage, Storage,
};

/// Storage whose settlement writes (rows that are no longer `Pending`) block
/// until released, announcing that they started.
struct GatedStorage {
    inner: MemoryStorage,
    entered: Notify,
    release: Semaphore,
}

impl GatedStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStorage::new(),
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }
}

#[async_trait]
impl Storage for GatedStorage {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        if row.accounting.dispatched && row.accounting.status != CostStatus::Pending {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        self.inner.save_request(row).await
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

fn gated_client(p: &Scripted, storage: &Arc<GatedStorage>) -> Arc<universal_ai::AiClient> {
    Arc::new(client(
        vec![p.clone()],
        ClientOpts {
            storage: Some(Arc::clone(storage) as Arc<dyn Storage>),
            ..Default::default()
        },
    ))
}

async fn settled(c: &universal_ai::AiClient) -> bool {
    let rows = c.list_ai_requests(usize::MAX).await.unwrap();
    !rows.is_empty()
        && rows
            .iter()
            .all(|r| r.accounting.status != CostStatus::Pending)
}

#[tokio::test]
async fn request_future_dropped_during_settlement_still_settles() {
    let p = Scripted::always("p", Step::Ok(Some(usage(10, 5))));
    let storage = GatedStorage::new();
    let c = gated_client(&p, &storage);

    let task = tokio::spawn({
        let c = Arc::clone(&c);
        async move { send(&c).await }
    });
    // The provider answered; the settlement write is in progress. The caller
    // gives up right now (client disconnect, outer timeout, aborted task).
    storage.entered.notified().await;
    task.abort();
    let _ = task.await;
    storage.release.add_permits(1);

    eventually(|| settled(&c)).await;
    let row = &c.list_ai_requests(usize::MAX).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::Actual);
    assert_eq!(row.accounting.charged_cost, Some(d(ACTUAL)));
    eventually(|| async { c.stats().all().await.requests == 1 }).await;
    assert_eq!(spent_today(&c).await, d(ACTUAL), "reservation released");
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn stream_dropped_during_final_settlement_still_settles() {
    let p = Scripted::always(
        "p",
        Step::Stream {
            items: vec![
                StreamItem::Text("hi"),
                StreamItem::Usage(usage(10, 5)),
                StreamItem::Done,
            ],
            hang_at_end: false,
        },
    );
    let storage = GatedStorage::new();
    let c = gated_client(&p, &storage);

    let mut stream = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await
        .unwrap();
    // Read everything; the end of the provider stream triggers settlement,
    // which blocks — the consumer drops the stream at that moment.
    let drained = tokio::time::timeout(Duration::from_millis(200), async {
        while stream.next().await.is_some() {}
    });
    tokio::select! {
        _ = drained => panic!("settlement should be blocked"),
        _ = storage.entered.notified() => {}
    }
    drop(stream);
    storage.release.add_permits(1);

    eventually(|| settled(&c)).await;
    let row = &c.list_ai_requests(usize::MAX).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::Actual);
    eventually(|| async { c.stats().all().await.requests == 1 }).await;
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, 1).await;
}
