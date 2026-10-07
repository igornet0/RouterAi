//! One budget per SQLite database, whoever writes to it: separate clients and
//! separate processes reserve atomically (`BEGIN IMMEDIATE` check + insert), so
//! two reservations can never be admitted on the same headroom.
//!
//! Prices: `message("hi")` + `max_tokens(100)` reserves **0.218**; a daily limit of
//! 1.0 therefore admits exactly 4 in-flight attempts in total.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common::*;
use futures::StreamExt;
use rust_decimal::Decimal;
use universal_ai::{
    AiError, BudgetPolicy, CostStatus, RequestId, RequestUsage, SqliteStorage, Storage,
};

const LIMIT: &str = "1.0";
const FITS: usize = 4; // floor(1.0 / 0.218)

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uai-shared-{name}-{}", RequestId::new()))
}

fn db_url(path: &Path) -> String {
    format!("sqlite://{}?mode=rwc", path.display())
}

/// Client on its own connection pool to `db`; every attempt hangs until its
/// deadline, so each admitted attempt keeps its reservation charged.
async fn hanging_client(db: &Path, timeout: Duration) -> (universal_ai::AiClient, Scripted) {
    let storage = Arc::new(SqliteStorage::connect(&db_url(db)).await.unwrap());
    let p = Scripted::always("p", Step::Hang);
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d(LIMIT)),
            storage: Some(storage),
            timeout,
            ..Default::default()
        },
    );
    (c, p)
}

async fn committed_today(db: &Path) -> Decimal {
    let s = SqliteStorage::connect(&db_url(db)).await.unwrap();
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    s.spend_in_window(None, today, today + chrono::Duration::days(1))
        .await
        .unwrap()
}

/// Sum of the rows' charges, scanned (the slow path the totals must agree with).
async fn rows_charge(db: &Path, scope: Option<&str>) -> Decimal {
    let s = SqliteStorage::connect(&db_url(db)).await.unwrap();
    s.list_requests(usize::MAX)
        .await
        .unwrap()
        .iter()
        .filter(|r| scope.is_none() || r.accounting.budget_scope.as_deref() == scope)
        .map(RequestUsage::budget_charge)
        .sum()
}

#[tokio::test]
async fn two_clients_on_one_database_share_one_budget() {
    let db = temp("clients.db");
    let (a, pa) = hanging_client(&db, Duration::from_millis(400)).await;
    let (b, pb) = hanging_client(&db, Duration::from_millis(400)).await;
    let results = futures::future::join_all((0..10).flat_map(|_| [send(&a), send(&b)])).await;

    let admitted = pa.calls() + pb.calls();
    assert_eq!(admitted, FITS, "each client alone would admit {FITS}");
    let refused = results
        .iter()
        .filter(|r| matches!(r, Err(AiError::DailyLimitExceeded { .. })))
        .count();
    assert_eq!(refused, 20 - FITS);
    assert_eq!(
        committed_today(&db).await,
        d(WORST) * Decimal::from(FITS as u64)
    );
    assert!(committed_today(&db).await <= d(LIMIT));
    assert_eq!(committed_today(&db).await, rows_charge(&db, None).await);
    let _ = std::fs::remove_file(db);
}

// ---------- separate processes ----------

/// Child process body (run only via `spawn_child`): fires `CHILD_REQUESTS`
/// hanging requests at the shared database and reports how many were sent.
#[tokio::test]
#[ignore = "child process for shared-budget tests"]
async fn shared_budget_child() {
    let Ok(db) = std::env::var("SHARED_DB") else {
        return;
    };
    let out = PathBuf::from(std::env::var("SHARED_OUT").unwrap());
    let go = PathBuf::from(std::env::var("SHARED_GO").unwrap());
    let (c, p) = hanging_client(Path::new(&db), Duration::from_millis(1500)).await;
    // Start together with the other children.
    while !go.exists() {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let _ = futures::future::join_all((0..6).map(|_| send(&c))).await;
    std::fs::write(out, p.calls().to_string()).unwrap();
}

fn spawn_child(db: &Path, out: &Path, go: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["shared_budget_child", "--exact", "--ignored", "--nocapture"])
        .env("SHARED_DB", db)
        .env("SHARED_OUT", out)
        .env("SHARED_GO", go)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn separate_processes_share_one_budget() {
    let db = temp("procs.db");
    // Create the schema once so the children do not race on the migration.
    drop(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    let go = temp("go");
    let outs: Vec<PathBuf> = (0..3).map(|i| temp(&format!("out{i}"))).collect();
    let children: Vec<Child> = outs.iter().map(|o| spawn_child(&db, o, &go)).collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&go, b"go").unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let admitted: usize = outs
        .iter()
        .map(|o| {
            std::fs::read_to_string(o)
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .sum();
    assert_eq!(admitted, FITS, "3 processes x 6 requests, one budget");
    assert_eq!(
        committed_today(&db).await,
        d(WORST) * Decimal::from(FITS as u64)
    );
    assert_eq!(committed_today(&db).await, rows_charge(&db, None).await);
    for p in outs.iter().chain([&go, &db]) {
        let _ = std::fs::remove_file(p);
    }
}

// ---------- totals == rows ----------

#[tokio::test]
async fn running_totals_equal_the_rows_after_mixed_traffic() {
    let db = temp("mixed.db");
    let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    // The message names the behavior (call indices also count retries).
    let p = Scripted::new("p", |_, req| {
        match req.messages[0].content.to_plain_text().as_str() {
            "ok" => Step::Ok(Some(usage(10, 5))),
            "hang" => Step::Hang,
            "503" => Step::Status(503),
            "no-usage" => Step::Ok(None),
            "network" => Step::Network,
            _ => Step::Stream {
                items: vec![
                    StreamItem::Text("x"),
                    StreamItem::Usage(usage(10, 5)),
                    StreamItem::Done,
                ],
                hang_at_end: false,
            },
        }
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d("100")),
            storage: Some(storage.clone()),
            max_attempts: 2,
            timeout: Duration::from_millis(30),
            ..Default::default()
        },
    );
    let kinds = ["ok", "hang", "503", "no-usage", "network", "stream"];
    for i in 0..24 {
        let kind = kinds[i % kinds.len()];
        let scope = if i % 2 == 0 { "agent:a" } else { "agent:b" };
        let builder = c
            .chat()
            .model("m")
            .message(kind)
            .max_tokens(100)
            .budget_scope(scope, Some(d("50")));
        if kind == "stream" {
            let mut s = builder.stream().await.unwrap();
            // Every other stream is dropped after its first event.
            if i % 12 == 5 {
                let _ = s.next().await;
            } else {
                while s.next().await.is_some() {}
            }
        } else {
            let _ = builder.send().await;
        }
    }
    // Let settlements spawned by dropped streams finish.
    eventually(|| async {
        c.list_ai_requests(usize::MAX)
            .await
            .unwrap()
            .iter()
            .all(|r| r.accounting.status != CostStatus::Pending)
    })
    .await;

    assert_invariants(&c, p.calls()).await;
    for scope in ["agent:a", "agent:b"] {
        assert_eq!(
            c.budget_status(Some(scope)).await.unwrap().daily_spent,
            rows_charge(&db, Some(scope)).await,
            "{scope}"
        );
    }
    // Fast path (period totals) == slow path (scan of an arbitrary window).
    let now = Utc::now();
    let scanned = storage
        .spend_in_window(
            None,
            now - chrono::Duration::hours(1),
            now + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    assert_eq!(scanned, committed_today(&db).await);
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn replayed_settlement_does_not_change_the_totals() {
    let db = temp("replay.db");
    let storage = SqliteStorage::connect(&db_url(&db)).await.unwrap();
    let c = client(
        vec![Scripted::always("p", Step::Ok(Some(usage(10, 5))))],
        ClientOpts {
            storage: Some(Arc::new(storage.clone())),
            ..Default::default()
        },
    );
    let r = send(&c).await.unwrap();
    let row = storage.get_request(&r.request_id).await.unwrap().unwrap();
    for _ in 0..3 {
        storage.save_request(&row).await.unwrap();
    }
    assert_eq!(committed_today(&db).await, d(ACTUAL));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn schema_v2_database_gets_totals_rebuilt_from_its_rows() {
    let db = temp("v2.db");
    {
        let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
        let c = client(
            vec![Scripted::new("p", |n, _| {
                if n == 0 {
                    Step::Ok(Some(usage(10, 5)))
                } else {
                    Step::Hang
                }
            })],
            ClientOpts {
                storage: Some(storage),
                timeout: Duration::from_millis(20),
                ..Default::default()
            },
        );
        send(&c).await.unwrap(); // 0.02 actual
        send(&c).await.unwrap_err(); // timeout: 0.218 kept
    }
    // Turn it into a v2 database (no running totals).
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url(&db))
        .await
        .unwrap();
    for sql in ["DROP TABLE spend_totals", "PRAGMA user_version = 2"] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let storage = SqliteStorage::connect(&db_url(&db)).await.unwrap();
    assert_eq!(
        storage.schema_version().await.unwrap(),
        universal_ai::storage::SQLITE_SCHEMA_VERSION
    );
    assert_eq!(committed_today(&db).await, d(ACTUAL) + d(WORST));
    assert_eq!(committed_today(&db).await, rows_charge(&db, None).await);
    let _ = std::fs::remove_file(db);
}
