//! Crash consistency with real process kills (SIGKILL) and SQLite migrations.
//!
//! Each scenario re-runs this test binary as a child (`crash_child`, ignored by
//! default), waits until the child reaches the interesting point, kills it with
//! SIGKILL, then "restarts" against the same database and checks that no
//! possibly-billed request became free.

#![cfg(all(feature = "sqlite", unix))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::*;
use rust_decimal::Decimal;
use universal_ai::{
    Account, AiResult, Balance, BudgetPolicy, CostStatus, RequestId, RequestUsage, SqliteStorage,
    Storage,
};

fn db_url(path: &Path) -> String {
    format!("sqlite://{}?mode=rwc", path.display())
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uai-crash-{name}-{}", RequestId::new()))
}

/// Blocks every settlement write (non-`Pending` row) after touching `marker`,
/// so the process can be killed after the provider answered but before the
/// settlement was persisted.
struct BlockSettlement {
    inner: SqliteStorage,
    marker: PathBuf,
}

#[async_trait]
impl Storage for BlockSettlement {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        if row.accounting.status != CostStatus::Pending {
            std::fs::write(&self.marker, b"settling").unwrap();
            futures::future::pending::<()>().await;
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

/// Child process body (run only via `spawn_child`).
#[tokio::test]
#[ignore = "child process for crash tests"]
async fn crash_child() {
    // Only meaningful when spawned by a crash test.
    let Ok(scenario) = std::env::var("CRASH_SCENARIO") else {
        return;
    };
    let db = PathBuf::from(std::env::var("CRASH_DB").unwrap());
    let marker = PathBuf::from(std::env::var("CRASH_MARKER").unwrap());
    let sqlite = SqliteStorage::connect(&db_url(&db)).await.unwrap();
    let opts = |storage: Arc<dyn Storage>| ClientOpts {
        budget: BudgetPolicy::daily_usd(d("10")),
        storage: Some(storage),
        timeout: Duration::from_secs(3600),
        ..Default::default()
    };
    match scenario.as_str() {
        // Reserved, request in flight forever.
        "reserve" => {
            let c = client(
                vec![Scripted::always("p", Step::Hang)],
                opts(Arc::new(sqlite)),
            );
            let _ = send(&c).await;
        }
        // Provider answered; settlement write never completes.
        "before_settlement" => {
            let storage = Arc::new(BlockSettlement {
                inner: sqlite,
                marker,
            });
            let c = client(
                vec![Scripted::always("p", Step::Ok(Some(usage(10, 5))))],
                opts(storage),
            );
            let _ = send(&c).await;
        }
        // Fully settled, then killed.
        "after_settlement" | "after_settlement_wal" => {
            if scenario.ends_with("_wal") {
                sqlite.enable_wal().await.unwrap();
            }
            let c = client(
                vec![Scripted::always("p", Step::Ok(Some(usage(10, 5))))],
                opts(Arc::new(sqlite)),
            );
            send(&c).await.unwrap();
            std::fs::write(&marker, b"settled").unwrap();
            futures::future::pending::<()>().await;
        }
        // Several reservations in flight at once.
        "concurrent" => {
            let c = client(
                vec![Scripted::always("p", Step::Hang)],
                opts(Arc::new(sqlite)),
            );
            let _ = futures::future::join_all((0..5).map(|_| send(&c))).await;
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn spawn_child(scenario: &str, db: &Path, marker: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--ignored", "--nocapture"])
        .env("CRASH_SCENARIO", scenario)
        .env("CRASH_DB", db)
        .env("CRASH_MARKER", marker)
        .spawn()
        .unwrap()
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("child did not reach the expected state");
}

async fn pending_rows(db: &Path) -> usize {
    let s = SqliteStorage::connect(&db_url(db)).await.unwrap();
    s.list_requests(usize::MAX)
        .await
        .unwrap()
        .iter()
        .filter(|r| r.accounting.status == CostStatus::Pending)
        .count()
}

/// Kill with SIGKILL (no destructors, no settlement).
fn sigkill(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

async fn restarted(db: &Path, limit: &str) -> (universal_ai::AiClient, Scripted) {
    let storage = Arc::new(SqliteStorage::connect(&db_url(db)).await.unwrap());
    let p = Scripted::always("p", Step::Ok(Some(usage(10, 5))));
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d(limit)),
            storage: Some(storage),
            ..Default::default()
        },
    );
    (c, p)
}

async fn wait_pending(db: &Path, n: usize) {
    for _ in 0..1_000 {
        if pending_rows(db).await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("child did not reserve");
}

#[tokio::test]
async fn kill_after_reserve_keeps_reservation_charged() {
    let (db, marker) = (temp("reserve.db"), temp("marker"));
    let child = spawn_child("reserve", &db, &marker);
    wait_pending(&db, 1).await;
    sigkill(child);

    let (c, p) = restarted(&db, "0.4").await;
    assert_eq!(
        spent_today(&c).await,
        d(WORST),
        "orphaned reservation still charged"
    );
    // 0.218 + 0.218 > 0.4: the restarted process must not treat it as free.
    send(&c).await.unwrap_err();
    assert_eq!(p.calls(), 0);

    // Explicit recovery changes the status, never the charge.
    let n = c.recover_orphaned_reservations(Utc::now()).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(spent_today(&c).await, d(WORST));
    let (c2, _) = restarted(&db, "0.4").await;
    assert_eq!(spent_today(&c2).await, d(WORST));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn kill_after_provider_success_before_settlement() {
    let (db, marker) = (temp("before.db"), temp("marker"));
    let child = spawn_child("before_settlement", &db, &marker);
    wait_for(|| marker.exists()).await;
    sigkill(child);

    let (c, _) = restarted(&db, "10").await;
    // The provider billed the request, the settlement was lost: the worst case
    // stays charged (never zero).
    assert_eq!(spent_today(&c).await, d(WORST));
    assert_eq!(pending_rows(&db).await, 1);
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn kill_after_settlement_keeps_actual_cost() {
    let (db, marker) = (temp("after.db"), temp("marker"));
    let child = spawn_child("after_settlement", &db, &marker);
    wait_for(|| marker.exists()).await;
    sigkill(child);

    let (c, _) = restarted(&db, "10").await;
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_eq!(pending_rows(&db).await, 0);
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn kill_after_settlement_in_wal_mode_keeps_actual_cost() {
    let (db, marker) = (temp("after-wal.db"), temp("marker"));
    let child = spawn_child("after_settlement_wal", &db, &marker);
    wait_for(|| marker.exists()).await;
    sigkill(child);

    let (c, _) = restarted(&db, "10").await;
    assert_eq!(
        spent_today(&c).await,
        d(ACTUAL),
        "WAL commits survive SIGKILL"
    );
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn kill_with_concurrent_reservations() {
    let (db, marker) = (temp("concurrent.db"), temp("marker"));
    let child = spawn_child("concurrent", &db, &marker);
    wait_pending(&db, 5).await;
    sigkill(child);

    let five = d(WORST) * Decimal::from(5);
    let (c, p) = restarted(&db, "1.2").await;
    assert_eq!(spent_today(&c).await, five);
    // 1.09 + 0.218 > 1.2.
    send(&c).await.unwrap_err();
    assert_eq!(p.calls(), 0);
    let _ = std::fs::remove_file(db);
}

// ---------- migrations ----------

async fn raw_pool(path: &Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url(path))
        .await
        .unwrap()
}

/// Schema of the first release (no content / accounting columns).
const V0_SCHEMA: &str = "CREATE TABLE requests (
    request_id TEXT PRIMARY KEY, provider TEXT NOT NULL, account TEXT NOT NULL,
    api_key TEXT, model TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT NOT NULL,
    prompt_tokens INTEGER NOT NULL, completion_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL, cost_amount TEXT, success INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL)";

#[tokio::test]
async fn migrates_v0_database_and_counts_legacy_costs() {
    let db = temp("v0.db");
    let pool = raw_pool(&db).await;
    sqlx::query(V0_SCHEMA).execute(&pool).await.unwrap();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO requests VALUES ('legacy-1','p','default',NULL,'m',?,?,10,5,15,'0.5',1,3)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let storage = SqliteStorage::connect(&db_url(&db)).await.unwrap();
    assert_eq!(
        storage.schema_version().await.unwrap(),
        universal_ai::storage::SQLITE_SCHEMA_VERSION
    );
    let rows = storage.list_requests(10).await.unwrap();
    assert_eq!(rows[0].accounting.status, CostStatus::Unknown);
    assert_eq!(
        rows[0].budget_charge(),
        d("0.5"),
        "legacy cost still counts"
    );

    // Re-opening is idempotent; the restarted ledger sees the legacy spend.
    drop(storage);
    let (c, _) = restarted(&db, "10").await;
    assert_eq!(spent_today(&c).await, d("0.5"));
    send(&c).await.unwrap();
    assert_eq!(spent_today(&c).await, d("0.52"));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn migrates_v1_pending_reservation_without_making_it_free() {
    let db = temp("v1.db");
    let pool = raw_pool(&db).await;
    sqlx::query(V0_SCHEMA).execute(&pool).await.unwrap();
    for col in [
        "request_json TEXT NOT NULL DEFAULT '{}'",
        "response_json TEXT",
        "importance INTEGER",
        "cost_status TEXT",
        "estimated_cost TEXT",
        "charged_cost TEXT",
        "budget_scope TEXT",
        "logical_request_id TEXT",
        "attempt INTEGER",
        "rejection TEXT",
    ] {
        sqlx::query(&format!("ALTER TABLE requests ADD COLUMN {col}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO requests (request_id, provider, account, model, started_at, finished_at, \
         prompt_tokens, completion_tokens, total_tokens, success, latency_ms, cost_status, \
         estimated_cost, charged_cost, attempt) \
         VALUES ('v1-pending','p','default','m',?,?,0,0,0,0,0,'pending','0.3','0.3',1)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let (c, _) = restarted(&db, "10").await;
    assert_eq!(spent_today(&c).await, d("0.3"));
    assert_eq!(
        c.recover_orphaned_reservations(Utc::now()).await.unwrap(),
        1
    );
    let rows = c.list_ai_requests(10).await.unwrap();
    assert_eq!(rows[0].accounting.status, CostStatus::Abandoned);
    assert_eq!(spent_today(&c).await, d("0.3"), "charge kept");
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn refuses_database_from_a_newer_schema() {
    let db = temp("future.db");
    let pool = raw_pool(&db).await;
    sqlx::query("PRAGMA user_version = 99")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let err = SqliteStorage::connect(&db_url(&db)).await.err().unwrap();
    assert_eq!(err.kind(), universal_ai::ErrorKind::Storage);
    assert!(err.to_string().contains("newer"));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn new_columns_round_trip() {
    let db = temp("roundtrip.db");
    let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    let p = Scripted::always(
        "p",
        Step::Ok(Some(universal_ai::Usage {
            cached_tokens: Some(4),
            reasoning_tokens: Some(2),
            ..usage(10, 5)
        })),
    );
    let c = client(
        vec![p],
        ClientOpts {
            storage: Some(storage.clone()),
            ..Default::default()
        },
    );
    let r = send(&c).await.unwrap();
    let stored = storage.get_request(&r.request_id).await.unwrap().unwrap();
    let memory = c.request_usage(&r.request_id).unwrap();
    assert_eq!(stored.accounting, memory.accounting);
    assert_eq!(stored.usage, memory.usage);
    assert_eq!(stored.cost, memory.cost);
    let _ = std::fs::remove_file(db);
}
