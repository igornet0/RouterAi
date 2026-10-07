//! Accounting layer on top of reserve → dispatch → settle: persisted attempt
//! lookup independent of the bounded in-memory rows, and reconciliation of a
//! settled attempt to what the provider later reported (refund or surcharge).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rust_decimal::Decimal;
use universal_ai::{
    AiClient, AiError, BudgetPolicy, CostStatus, ErrorKind, Reconciliation, RequestId,
    SqliteStorage, Storage,
};

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uai-accounting-{name}-{}", RequestId::new()))
}

fn db_url(path: &Path) -> String {
    format!("sqlite://{}?mode=rwc", path.display())
}

async fn sqlite_client(db: &Path, p: &Scripted, opts: ClientOpts) -> AiClient {
    let storage = Arc::new(SqliteStorage::connect(&db_url(db)).await.unwrap());
    client(
        vec![p.clone()],
        ClientOpts {
            storage: Some(storage),
            ..opts
        },
    )
}

fn by_usage(source: &str) -> Reconciliation {
    Reconciliation::Usage {
        usage: usage(10, 5),
        source: source.into(),
    }
}

fn by_amount(amount: &str, source: &str) -> Reconciliation {
    Reconciliation::Amount {
        amount: d(amount),
        source: source.into(),
    }
}

/// Every persisted row's charge, summed (what the totals must equal).
async fn rows_charge(c: &AiClient) -> Decimal {
    c.list_ai_requests(usize::MAX)
        .await
        .unwrap()
        .iter()
        .map(universal_ai::RequestUsage::budget_charge)
        .sum()
}

// ---------- persisted attempts ----------

#[tokio::test]
async fn logical_attempts_are_loaded_from_storage_after_eviction_and_restart() {
    let db = temp("attempts.db");
    let p = Scripted::new("p", |n, _| match n {
        0 | 1 => Step::Hang,
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    let c = universal_ai::AiClient::builder()
        .provider(p.clone())
        .recent_attempts(1)
        .config(universal_ai::AiConfig {
            budget: BudgetPolicy::daily_usd(d("10")),
            default_timeout: Duration::from_millis(20),
            retry_policy: universal_ai::RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
                exponential_backoff: false,
            },
            ..Default::default()
        })
        .storage(storage)
        .build()
        .unwrap();
    c.pricing().upsert(price("p", "m"));
    let r = send(&c).await.unwrap();

    // Memory keeps one row; storage has the whole logical request.
    assert!(c.logical_request_attempts(&r.request_id).len() <= 1);
    let attempts = c
        .load_logical_request_attempts(&r.request_id)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 3);
    assert_eq!(
        attempts
            .iter()
            .map(|a| a.accounting.attempt)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let charged: Decimal = attempts.iter().map(|a| a.budget_charge()).sum();
    assert_eq!(
        charged,
        d(WORST) * Decimal::from(2) + d(ACTUAL),
        "timeouts kept"
    );
    // Any attempt id finds the whole logical request.
    let again = c
        .load_logical_request_attempts(&attempts[0].request_id)
        .await
        .unwrap();
    assert_eq!(again.len(), 3);
    drop(c);

    // A new process sees the same.
    let c = sqlite_client(&db, &p, ClientOpts::default()).await;
    let after_restart = c
        .load_logical_request_attempts(&r.request_id)
        .await
        .unwrap();
    assert_eq!(after_restart.len(), 3);
    assert!(c
        .load_logical_request_attempts(&RequestId::new())
        .await
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_file(db);
}

// ---------- reconciliation ----------

/// A timed-out attempt is charged its worst case; the provider's usage export
/// later shows 10 in / 5 out: the attempt is reconciled to 0.02 and the
/// difference is released from the budget.
async fn refund_case(c: &AiClient) {
    let err = send(c).await.unwrap_err();
    assert!(matches!(err, AiError::Timeout));
    let id = c.list_ai_requests(1).await.unwrap()[0].request_id;
    assert_eq!(spent_today(c).await, d(WORST));

    let row = c
        .reconcile_attempt(&id, by_usage("provider usage export 2026-10-07"))
        .await
        .unwrap();
    assert_eq!(row.accounting.status, CostStatus::Reconciled);
    assert_eq!(row.accounting.charged_cost, Some(d(ACTUAL)));
    assert_eq!(row.usage, usage(10, 5));
    let note = row.accounting.cost_note.clone().unwrap();
    assert!(
        note.starts_with("reconciled from provider usage export"),
        "{note}"
    );
    assert!(note.contains("UsageUnavailable charged 0.218"), "{note}");
    assert!(note.contains("adjustment -0.198"), "{note}");

    assert_eq!(spent_today(c).await, d(ACTUAL), "refund released");
    assert_eq!(c.stats().all().await.charged_cost, d(ACTUAL));
    assert_eq!(rows_charge(c).await, d(ACTUAL));

    // Replaying the same reconciliation changes nothing.
    let replay = c
        .reconcile_attempt(&id, by_usage("provider usage export 2026-10-07"))
        .await
        .unwrap();
    assert_eq!(replay.accounting, row.accounting);
    assert_eq!(spent_today(c).await, d(ACTUAL));

    // A later invoice corrects it again (surcharge).
    c.reconcile_attempt(&id, by_amount("0.05", "invoice INV-1"))
        .await
        .unwrap();
    assert_eq!(spent_today(c).await, d("0.05"));
    assert_eq!(c.stats().all().await.charged_cost, d("0.05"));
}

#[tokio::test]
async fn reconciliation_refunds_and_corrects_with_sqlite() {
    let db = temp("refund.db");
    let p = Scripted::always("p", Step::Hang);
    let opts = ClientOpts {
        timeout: Duration::from_millis(20),
        ..Default::default()
    };
    let c = sqlite_client(&db, &p, opts).await;
    refund_case(&c).await;
    assert_invariants(&c, 1).await;
    drop(c);

    // Persisted: a new process sees the reconciled charge.
    let c = sqlite_client(&db, &p, ClientOpts::default()).await;
    assert_eq!(spent_today(&c).await, d("0.05"));
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::Reconciled);
    assert_eq!(row.cost.as_ref().unwrap().amount, d("0.05"));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn reconciliation_refunds_and_corrects_in_memory() {
    let p = Scripted::always("p", Step::Hang);
    let c = client(
        vec![p],
        ClientOpts {
            timeout: Duration::from_millis(20),
            ..Default::default()
        },
    );
    refund_case(&c).await;
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn a_failure_later_found_billed_is_charged() {
    let p = Scripted::always("p", Step::Status(500));
    let c = client(vec![p.clone()], ClientOpts::default());
    send(&c).await.unwrap_err();
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::NotCharged);
    c.reconcile_attempt(&row.request_id, by_amount("0.07", "invoice INV-2"))
        .await
        .unwrap();
    assert_eq!(spent_today(&c).await, d("0.07"));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn reconciliation_refuses_what_it_cannot_do_safely() {
    let p = Scripted::new("p", |n, _| match n {
        0 => Step::Hang,
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let c = Arc::new(client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d("0.3")),
            ..Default::default()
        },
    ));

    // In flight: still Pending — never reconciled underneath its settlement.
    let inflight = tokio::spawn({
        let c = Arc::clone(&c);
        async move { send(&c).await }
    });
    eventually(|| async { !c.list_ai_requests(1).await.unwrap().is_empty() }).await;
    let pending = c.list_ai_requests(1).await.unwrap()[0].clone();
    assert_eq!(pending.accounting.status, CostStatus::Pending);
    let err = c
        .reconcile_attempt(&pending.request_id, by_usage("export"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");

    // Rejected by the budget gate (0.218 reserved + 0.218 > 0.3): never sent.
    let rejected_err = send(&c).await.unwrap_err();
    assert!(matches!(rejected_err, AiError::DailyLimitExceeded { .. }));
    let rejected = c
        .list_ai_requests(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.accounting.status == CostStatus::Rejected)
        .unwrap();
    let err = c
        .reconcile_attempt(&rejected.request_id, by_amount("0.01", "invoice"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    inflight.abort();

    // Settle something to reconcile against.
    let c2 = client(vec![p.clone()], ClientOpts::default());
    let ok = send(&c2).await.unwrap();
    let id = ok.request_id;
    for (with, kind) in [
        (by_amount("-0.01", "invoice"), ErrorKind::Validation),
        (by_amount("0.01", "  "), ErrorKind::Validation),
        (
            Reconciliation::Usage {
                usage: universal_ai::Usage {
                    other_tokens: [("input_audio".to_string(), 3)].into(),
                    ..usage(10, 5)
                },
                source: "export".into(),
            },
            ErrorKind::Pricing,
        ),
    ] {
        let err = c2.reconcile_attempt(&id, with).await.unwrap_err();
        assert_eq!(err.kind(), kind, "{err}");
    }
    let err = c2
        .reconcile_attempt(&RequestId::new(), by_amount("0.01", "invoice"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(spent_today(&c2).await, d(ACTUAL), "refusals change nothing");
    assert_invariants(&c2, p.calls() - 1).await;
}

#[tokio::test]
async fn reconciled_rows_older_than_the_recent_window_still_move_the_ledger() {
    let db = temp("old.db");
    let p = Scripted::always("p", Step::Ok(None));
    let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    let c = universal_ai::AiClient::builder()
        .provider(p.clone())
        .recent_attempts(1)
        .budget(BudgetPolicy::daily_usd(d("10")))
        .storage(storage)
        .build()
        .unwrap();
    c.pricing().upsert(price("p", "m"));
    let first = send(&c).await.unwrap().request_id; // no usage: 0.218 kept
    send(&c).await.unwrap(); // evicts the first row from memory
    assert!(c.request_usage(&first).is_none());
    c.reconcile_attempt(&first, by_usage("export"))
        .await
        .unwrap();
    assert_eq!(spent_today(&c).await, d(ACTUAL) + d(WORST));
    assert_eq!(rows_charge(&c).await, spent_today(&c).await);
    let _ = std::fs::remove_file(db);
}
