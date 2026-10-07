//! Financial invariants of the single execution path (deterministic in-process
//! provider; no network).
//!
//! Every test ends with [`assert_invariants`]:
//! `sum(charged) == persisted spend`, `settled == actual`, `rejected ⇒ not sent`,
//! one row per physical attempt, nothing left `Pending`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures::StreamExt;
use rust_decimal::Decimal;
use universal_ai::{
    AiError, BudgetPolicy, CostStatus, ErrorKind, MissingUsagePolicy, RequestUsage, StreamEvent,
};

fn rows_by_attempt(rows: &mut [RequestUsage]) {
    rows.sort_by_key(|r| r.accounting.attempt);
}

async fn rows(client: &universal_ai::AiClient) -> Vec<RequestUsage> {
    let mut rows = client.list_ai_requests(usize::MAX).await.unwrap();
    rows_by_attempt(&mut rows);
    rows
}

// ---------- normal ----------

#[tokio::test]
async fn normal_request_settles_actual_and_releases_reservation() {
    let p = Scripted::always("p", Step::Ok(Some(usage(10, 5))));
    let c = client(vec![p.clone()], ClientOpts::default());
    let r = send(&c).await.unwrap();
    assert_eq!(r.cost.unwrap().amount, d(ACTUAL));
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.reserved_cost, Some(d(WORST)));
    assert_eq!(row.accounting.charged_cost, Some(d(ACTUAL)));
    assert!(row.accounting.dispatched);
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, p.calls()).await;
}

// ---------- retry: one reservation and one settlement per physical attempt ----------

#[tokio::test]
async fn retries_are_separate_attempts_with_own_reservation() {
    let p = Scripted::new("p", |n, _| match n {
        0 | 1 => Step::Status(503),
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            max_attempts: 3,
            ..Default::default()
        },
    );
    let response = send(&c).await.unwrap();
    assert_eq!(p.calls(), 3);
    let rows = rows(&c).await;
    assert_eq!(rows.len(), 3, "one row per physical attempt");
    let logical = rows[0].accounting.logical_request_id;
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.accounting.attempt, i as u32 + 1);
        assert_eq!(r.accounting.retry, i as u32);
        assert_eq!(r.accounting.logical_request_id, logical);
        assert_eq!(
            r.accounting.reserved_cost,
            Some(d(WORST)),
            "own reservation"
        );
    }
    assert_eq!(rows[0].accounting.status, CostStatus::NotCharged);
    assert_eq!(rows[1].accounting.status, CostStatus::NotCharged);
    assert_eq!(rows[2].accounting.status, CostStatus::Actual);
    assert_eq!(rows[2].request_id, response.request_id);
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, 3).await;
}

/// Regression (P0): a timed-out attempt that is retried used to share one
/// reservation with the retry, so only the last attempt was charged.
#[tokio::test]
async fn retry_after_timeout_keeps_the_ambiguous_charge() {
    let p = Scripted::new("p", |n, _| match n {
        0 => Step::Hang,
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            max_attempts: 2,
            timeout: Duration::from_millis(30),
            ..Default::default()
        },
    );
    send(&c).await.unwrap();
    let rows = rows(&c).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(rows[0].accounting.error_kind, Some(ErrorKind::Timeout));
    assert_eq!(rows[0].accounting.charged_cost, Some(d(WORST)));
    assert_eq!(
        spent_today(&c).await,
        d(WORST) + d(ACTUAL),
        "no hidden cost"
    );
    assert_invariants(&c, 2).await;
}

#[tokio::test]
async fn budget_exhaustion_stops_further_attempts() {
    // Each timed-out attempt keeps 0.218 charged: the third reservation would
    // exceed 0.5 and must be rejected before any HTTP attempt.
    let p = Scripted::always("p", Step::Hang);
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::daily_usd(d("0.5")),
            max_attempts: 5,
            timeout: Duration::from_millis(20),
            ..Default::default()
        },
    );
    let err = send(&c).await.unwrap_err();
    assert!(matches!(err, AiError::DailyLimitExceeded { .. }), "{err:?}");
    assert_eq!(err.kind(), ErrorKind::Budget);
    assert_eq!(p.calls(), 2, "no attempt after budget exhaustion");
    let rows = rows(&c).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].accounting.status, CostStatus::Rejected);
    assert!(spent_today(&c).await <= d("0.5"));
    assert_invariants(&c, 2).await;
}

/// Regression: `max_cost` used to be checked per attempt, so a retry after a
/// charged (timed-out) attempt could take the logical request over its cap.
#[tokio::test]
async fn max_cost_caps_the_logical_request_across_retries() {
    let p = Scripted::new("p", |n, _| match n {
        0 => Step::Hang,
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            max_attempts: 3,
            timeout: Duration::from_millis(20),
            ..Default::default()
        },
    );
    let id = universal_ai::RequestId::new();
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .max_cost(d("0.3"))
        .request_id(id)
        .send()
        .await
        .unwrap_err();
    // 0.218 charged by the timed-out attempt + 0.218 worst case > 0.3.
    assert!(matches!(err, AiError::BudgetExceeded { .. }), "{err:?}");
    assert!(err.to_string().contains("already charged"));
    assert_eq!(p.calls(), 1);
    let attempts = c.logical_request_attempts(&id);
    assert_eq!(attempts.len(), 2, "timed-out attempt + rejected retry");
    let total: Decimal = attempts.iter().map(RequestUsage::budget_charge).sum();
    assert!(total <= d("0.3"));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn logical_request_attempts_are_correlated() {
    let p = Scripted::new("p", |n, _| match n {
        0 => Step::Status(503),
        _ => Step::Ok(Some(usage(10, 5))),
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            max_attempts: 2,
            ..Default::default()
        },
    );
    let r = send(&c).await.unwrap();
    let attempts = c.logical_request_attempts(&r.request_id);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].request_id, r.request_id);
    assert_eq!(
        attempts[0].accounting.logical_request_id,
        attempts[1].accounting.logical_request_id
    );
}

#[tokio::test]
async fn rejected_requests_make_no_http_attempt() {
    let p = Scripted::always("p", Step::Ok(Some(usage(10, 5))));
    let c = client(vec![p.clone()], ClientOpts::default());
    // Unknown price.
    let err = c
        .chat()
        .model("unpriced")
        .message("hi")
        .max_tokens(10)
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Pricing);
    // Unbounded output.
    let err = c.chat().model("m").message("hi").send().await.unwrap_err();
    assert!(matches!(err, AiError::OutputLimitUnknown { .. }));
    // Per-request cap.
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .max_cost(d("0.01"))
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Budget);
    assert_eq!(p.calls(), 0);
    assert_eq!(spent_today(&c).await, Decimal::ZERO);
    assert_invariants(&c, 0).await;
}

#[tokio::test]
async fn non_retryable_errors_are_not_retried() {
    for step in [Step::Status(400), Step::Status(401), Step::Malformed] {
        let p = Scripted::always("p", step);
        let c = client(
            vec![p.clone()],
            ClientOpts {
                max_attempts: 5,
                ..Default::default()
            },
        );
        send(&c).await.unwrap_err();
        assert_eq!(p.calls(), 1);
        assert_invariants(&c, 1).await;
    }
}

// ---------- fallback ----------

#[tokio::test]
async fn fallback_after_malformed_response_keeps_both_charges() {
    let a = Scripted::always("a", Step::Malformed);
    let b = Scripted::always("b", Step::Ok(Some(usage(10, 5))));
    let c = client(
        vec![a.clone(), b.clone()],
        ClientOpts {
            fallback: true,
            max_attempts: 3,
            ..Default::default()
        },
    );
    let r = send(&c).await.unwrap();
    assert_eq!(a.calls(), 1, "malformed responses are not retried");
    let rows = rows(&c).await;
    assert_eq!(rows[0].accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(rows[1].request_id, r.request_id);
    assert_eq!(spent_today(&c).await, d(WORST) + d(ACTUAL));
    assert_invariants(&c, 2).await;
}

#[tokio::test]
async fn fallback_policy_by_error_class() {
    // (step on provider a, falls back to b?)
    for (step, falls_back) in [
        (Step::Status(401), true),
        (Step::Status(429), true),
        (Step::Status(503), true),
        (Step::Network, true),
        (Step::Status(400), false),
        (Step::Status(422), false),
    ] {
        let a = Scripted::always("a", step);
        let b = Scripted::always("b", Step::Ok(Some(usage(10, 5))));
        let c = client(
            vec![a.clone(), b.clone()],
            ClientOpts {
                fallback: true,
                ..Default::default()
            },
        );
        let result = send(&c).await;
        assert_eq!(result.is_ok(), falls_back);
        assert_eq!(b.calls(), usize::from(falls_back));
        assert_invariants(&c, a.calls() + b.calls()).await;
    }
}

#[tokio::test]
async fn side_effecting_requests_never_fall_back() {
    let a = Scripted::always("a", Step::Status(503));
    let b = Scripted::always("b", Step::Ok(Some(usage(10, 5))));
    let c = client(
        vec![a.clone(), b.clone()],
        ClientOpts {
            fallback: true,
            ..Default::default()
        },
    );
    c.chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .side_effecting(true)
        .send()
        .await
        .unwrap_err();
    assert_eq!(b.calls(), 0);
}

// ---------- cancellation ----------

#[tokio::test]
async fn cancel_signal_settles_abandoned_and_keeps_the_reservation() {
    let p = Scripted::always("p", Step::Hang);
    let c = client(vec![p.clone()], ClientOpts::default());
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .cancel_on(tokio::time::sleep(Duration::from_millis(20)))
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Cancelled));
    assert_eq!(err.kind(), ErrorKind::Cancellation);
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.status, CostStatus::Abandoned);
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn dropped_request_future_is_settled_not_left_pending() {
    let p = Scripted::always("p", Step::Hang);
    let c = client(vec![p.clone()], ClientOpts::default());
    // The caller gives up (e.g. an agent run deadline) and drops the future.
    let _ = tokio::time::timeout(Duration::from_millis(20), send(&c)).await;
    eventually(|| async {
        rows(&c).await.first().map(|r| r.accounting.status) == Some(CostStatus::Abandoned)
    })
    .await;
    assert_eq!(
        spent_today(&c).await,
        d(WORST),
        "possibly billed: still charged"
    );
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn attempt_timeout_is_typed_and_charged() {
    let p = Scripted::always("p", Step::Hang);
    let c = client(vec![p.clone()], ClientOpts::default());
    let err = c
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .timeout(Duration::from_millis(10))
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Timeout));
    assert_eq!(spent_today(&c).await, d(WORST));
    assert_invariants(&c, 1).await;
}

// ---------- streaming ----------

fn stream_provider(items: Vec<StreamItem>, hang_at_end: bool) -> Scripted {
    Scripted::new("p", move |_, _| Step::Stream {
        items: items.clone(),
        hang_at_end,
    })
}

async fn open(c: &universal_ai::AiClient) -> universal_ai::ChatStream {
    c.chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await
        .unwrap()
}

async fn drain(mut s: universal_ai::ChatStream) -> Vec<Result<StreamEvent, AiError>> {
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    out
}

#[tokio::test]
async fn stream_completed_with_usage_settles_actual_once() {
    let storage = Arc::new(CheckingStorage::new());
    let p = stream_provider(
        vec![
            StreamItem::Text("a"),
            StreamItem::Usage(usage(10, 5)),
            StreamItem::Done,
        ],
        false,
    );
    let c = client(
        vec![p.clone()],
        ClientOpts {
            storage: Some(storage.clone()),
            ..Default::default()
        },
    );
    let s = open(&c).await;
    let events = drain(s).await;
    assert!(events.iter().all(Result::is_ok));
    // Settled before the consumer saw the end of the stream.
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.status, CostStatus::Actual);
    assert_eq!(row.accounting.charged_cost, Some(d(ACTUAL)));
    assert_eq!(
        storage.saves_of(&row.request_id),
        2,
        "reserve + exactly one settlement"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        storage.saves_of(&row.request_id),
        2,
        "no second settlement on drop"
    );
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn stream_duplicate_usage_keeps_the_maximum() {
    let p = stream_provider(
        vec![
            StreamItem::Usage(usage(10, 2)),
            StreamItem::Usage(usage(10, 5)),
            StreamItem::Usage(usage(10, 3)),
            StreamItem::Done,
        ],
        false,
    );
    let c = client(vec![p.clone()], ClientOpts::default());
    drain(open(&c).await).await;
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn stream_without_usage_keeps_the_reservation() {
    let p = stream_provider(vec![StreamItem::Text("a"), StreamItem::Done], false);
    let c = client(vec![p.clone()], ClientOpts::default());
    drain(open(&c).await).await;
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn stream_without_usage_can_be_rejected_by_policy() {
    let p = stream_provider(vec![StreamItem::Text("a"), StreamItem::Done], false);
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy {
                max_daily_cost: Some(d("10")),
                missing_usage: MissingUsagePolicy::Reject,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let events = drain(open(&c).await).await;
    assert!(matches!(
        events.last(),
        Some(Err(AiError::UsageUnavailable { .. }))
    ));
    assert_eq!(spent_today(&c).await, d(WORST));
}

#[tokio::test]
async fn stream_network_error_keeps_the_reservation() {
    let p = stream_provider(vec![StreamItem::Text("a"), StreamItem::NetworkError], false);
    let c = client(vec![p.clone()], ClientOpts::default());
    let events = drain(open(&c).await).await;
    assert!(events.last().unwrap().is_err());
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(row.accounting.error_kind, Some(ErrorKind::Network));
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn dropped_stream_closes_provider_stream_and_settles_abandoned() {
    let p = stream_provider(vec![StreamItem::Text("a")], true);
    let c = client(vec![p.clone()], ClientOpts::default());
    let mut s = open(&c).await;
    assert!(matches!(
        s.next().await,
        Some(Ok(StreamEvent::TextDelta { .. }))
    ));
    drop(s);
    // No background task keeps the provider stream (connection) alive.
    assert_eq!(
        p.streams_dropped.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    eventually(|| async { rows(&c).await[0].accounting.status == CostStatus::Abandoned }).await;
    assert_eq!(spent_today(&c).await, d(WORST));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn dropped_stream_after_final_usage_is_charged_actual() {
    let p = stream_provider(vec![StreamItem::Usage(usage(10, 5))], true);
    let c = client(vec![p.clone()], ClientOpts::default());
    let mut s = open(&c).await;
    s.next().await;
    drop(s);
    eventually(|| async { rows(&c).await[0].accounting.status == CostStatus::Abandoned }).await;
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn stream_open_failure_retries_with_new_reservation() {
    let p = Scripted::new("p", |n, _| match n {
        0 => Step::Status(503),
        _ => Step::Stream {
            items: vec![StreamItem::Usage(usage(10, 5)), StreamItem::Done],
            hang_at_end: false,
        },
    });
    let c = client(
        vec![p.clone()],
        ClientOpts {
            max_attempts: 2,
            ..Default::default()
        },
    );
    drain(open(&c).await).await;
    assert_eq!(rows(&c).await.len(), 2);
    assert_eq!(spent_today(&c).await, d(ACTUAL));
    assert_invariants(&c, 2).await;
}

// ---------- uncontrolled requests: unknown is still not zero ----------

#[tokio::test]
async fn unknown_cost_without_budget_charges_the_estimate() {
    let p = Scripted::always("p", Step::Ok(None));
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: BudgetPolicy::default(),
            ..Default::default()
        },
    );
    send(&c).await.unwrap();
    let row = &rows(&c).await[0];
    assert_eq!(row.accounting.reserved_cost, None, "nothing reserved");
    assert_eq!(row.accounting.charged_cost, Some(d(WORST)));
    assert_eq!(c.stats().all().await.unknown_cost_requests, 1);
    assert_invariants(&c, 1).await;
}

// ---------- property: sequential random outcomes never overspend ----------

#[tokio::test]
async fn random_sequences_never_overspend() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let limit = Decimal::new(rng.below(150) as i64 + 20, 2); // 0.20 ..= 1.69
        let outcomes: Vec<u64> = (0..64).map(|_| rng.below(6)).collect();
        let p = Scripted::new("p", move |n, _| match outcomes[n % outcomes.len()] {
            0 => Step::Ok(Some(usage(10, 5))),
            1 => Step::Ok(None),
            2 => Step::Status(503),
            3 => Step::Network,
            4 => Step::Malformed,
            _ => Step::Ok(Some(usage(1, 100))),
        });
        let storage = Arc::new(CheckingStorage::new());
        let c = client(
            vec![p.clone()],
            ClientOpts {
                budget: BudgetPolicy::daily_usd(limit),
                max_attempts: 2,
                storage: Some(storage.clone()),
                ..Default::default()
            },
        );
        for _ in 0..12 {
            let _ = send(&c).await;
        }
        assert!(
            storage.max_committed() <= limit,
            "seed {seed}: committed {} > limit {limit}",
            storage.max_committed()
        );
        assert_invariants(&c, p.calls()).await;
    }
}

// ---------- provider billed beyond the reserved bounds ----------

/// The worst case assumes the provider cannot bill more tokens than reserved
/// (input <= UTF-8 bytes + overhead, output <= `max_tokens`). A provider that does
/// (e.g. reasoning billed outside `max_tokens`) is charged its actual cost, and
/// the assumption is treated as falsified for that model: further
/// budget-controlled requests to it are rejected before any HTTP.
#[tokio::test]
async fn usage_beyond_reserved_bounds_blocks_further_budgeted_requests() {
    // max_tokens(100), but 500 output tokens billed: actual 0.01 + 1.0 > 0.218.
    let p = Scripted::always("p", Step::Ok(Some(usage(10, 500))));
    let c = client(vec![p.clone()], ClientOpts::default());
    let first = send(&c).await.unwrap();
    let overrun = d("1.01");
    assert_eq!(
        first.cost.unwrap().amount,
        overrun,
        "charged actual, not capped"
    );
    assert_eq!(spent_today(&c).await, overrun);

    let err = send(&c).await.unwrap_err();
    assert!(matches!(err, AiError::WorstCaseUnbounded { .. }), "{err:?}");
    assert_eq!(err.kind(), ErrorKind::Pricing);
    assert_eq!(err.budget_reason(), Some("worst_case_unbounded"));
    assert_eq!(p.calls(), 1, "rejected before HTTP");
    assert_eq!(spent_today(&c).await, overrun);
    assert_invariants(&c, 1).await;
}

#[tokio::test]
async fn input_beyond_byte_bound_blocks_the_model_but_not_others() {
    // "hi" has an input bound of 18 tokens; 1000 billed input tokens falsify it.
    let p = Scripted::new("p", |_, req| {
        if req.model.as_str() == "m" {
            Step::Ok(Some(usage(1000, 5)))
        } else {
            Step::Ok(Some(usage(10, 5)))
        }
    });
    let c = client(vec![p.clone()], ClientOpts::default());
    c.pricing().upsert(price("p", "other"));
    send(&c).await.unwrap();
    assert!(matches!(
        send(&c).await.unwrap_err(),
        AiError::WorstCaseUnbounded { .. }
    ));
    // Another model on the same provider is unaffected.
    c.chat()
        .model("other")
        .message("hi")
        .max_tokens(100)
        .send()
        .await
        .unwrap();
    assert_eq!(p.calls(), 2);
    assert_invariants(&c, 2).await;
}

#[tokio::test]
async fn usage_within_bounds_never_blocks() {
    // Exactly at the bounds: 18 input tokens ("hi" bound), 100 output tokens.
    let p = Scripted::always("p", Step::Ok(Some(usage(18, 100))));
    let c = client(vec![p.clone()], ClientOpts::default());
    for _ in 0..3 {
        let r = send(&c).await.unwrap();
        assert_eq!(r.cost.unwrap().amount, d(WORST), "worst case is tight");
    }
    assert_eq!(p.calls(), 3);
    assert_invariants(&c, 3).await;
}
