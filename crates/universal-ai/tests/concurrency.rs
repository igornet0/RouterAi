//! Concurrent reservations cannot oversubscribe the budget.
//!
//! The invariant is checked without timing assumptions: the storage wrapper
//! records the maximum committed spend (settled + reserved) ever persisted, and the
//! provider re-checks it at the moment of every dispatch.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures::StreamExt;
use rust_decimal::Decimal;
use universal_ai::{AiClient, BudgetPolicy, Storage};

/// Outcome chosen from the request text (`case-<k>`), so it is deterministic.
fn mixed(_: usize, req: &universal_ai::ChatRequest) -> Step {
    let text = req.messages[0].content.to_plain_text();
    let k: u64 = text.trim_start_matches("case-").parse().unwrap_or(0);
    match k % 6 {
        0 | 1 => Step::Delayed(Duration::from_millis(2), Some(usage(10, 5))),
        2 => Step::Hang, // times out
        3 => Step::Status(503),
        4 => Step::Delayed(Duration::from_millis(1), None),
        _ => Step::Stream {
            items: vec![
                StreamItem::Text("x"),
                StreamItem::Usage(usage(10, 5)),
                StreamItem::Done,
            ],
            hang_at_end: false,
        },
    }
}

async fn run_case(c: &AiClient, k: u64) {
    let builder = c
        .chat()
        .model("m")
        .message(format!("case-{k}"))
        .max_tokens(100);
    if k % 6 == 5 {
        if let Ok(mut s) = builder.stream().await {
            while s.next().await.is_some() {}
        }
    } else {
        let _ = builder.send().await;
    }
}

async fn stress(n: u64, limit: &str, seed: u64) {
    let limit = d(limit);
    let storage = Arc::new(CheckingStorage::new());
    let violated = Arc::new(AtomicBool::new(false));
    let mut provider = Scripted::new("p", mixed);
    {
        let storage = Arc::clone(&storage);
        let violated = Arc::clone(&violated);
        provider.on_call = Some(Arc::new(move || {
            // total reserved <= budget at the moment a request is dispatched.
            if storage.committed_now() > limit {
                violated.store(true, Ordering::SeqCst);
            }
        }));
    }
    let c = client(
        vec![provider.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(limit),
            max_attempts: 2,
            timeout: Duration::from_millis(15),
            storage: Some(storage.clone() as Arc<dyn Storage>),
            ..Default::default()
        },
    );
    let mut rng = Rng(seed);
    let cases: Vec<u64> = (0..n).map(|_| rng.below(1_000)).collect();
    futures::future::join_all(cases.iter().map(|k| run_case(&c, *k))).await;
    // Abandoned / spawned settlements, if any, finish quickly.
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(
        !violated.load(Ordering::SeqCst),
        "n={n}: dispatched over budget"
    );
    assert!(
        storage.max_committed() <= limit,
        "n={n}: committed {} > limit {limit}",
        storage.max_committed()
    );
    assert_invariants(&c, provider.calls()).await;
    let spent = spent_today(&c).await;
    assert!(spent <= limit);
    assert!(spent > Decimal::ZERO, "something was admitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent() {
    stress(10, "0.5", 11).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_concurrent() {
    stress(50, "0.7", 22).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundred_concurrent() {
    stress(100, "1.0", 33).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_hundred_concurrent() {
    stress(500, "2.0", 44).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn randomized_mixes() {
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
        let n = 20 + rng.below(60);
        let limit = format!("0.{}", 25 + rng.below(70));
        stress(n, &limit, rng.next()).await;
    }
}

/// Exactly the number of worst cases that fit is admitted while all are in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admits_exactly_what_fits() {
    let p = Scripted::always(
        "p",
        Step::Delayed(Duration::from_millis(50), Some(usage(10, 5))),
    );
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d("1.0")),
            ..Default::default()
        },
    );
    // 1.0 / 0.218 → 4 reservations fit at once.
    let results = futures::future::join_all((0..100).map(|_| send(&c))).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 4);
    assert_eq!(p.calls(), 4);
    assert_invariants(&c, 4).await;
}
