//! Tiered prices and versioned price sheets end to end: the reservation covers
//! the most expensive reachable tier, settlement uses the tier the usage
//! reached (the difference is released), and an attempt is settled and later
//! reconciled with the price sheet it was reserved with — across registry
//! changes, restarts and the schema v3 → v4 migration.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rust_decimal::Decimal;
use universal_ai::{
    AiClient, ChatRequest, CostStatus, ModelPricing, PriceTier, ProviderId, Reconciliation,
    RequestId, SqliteStorage, Storage, TierMode, Tiering,
};

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("uai-tiered-{name}-{}", RequestId::new()))
}

fn db_url(path: &Path) -> String {
    format!("sqlite://{}?mode=rwc", path.display())
}

/// Base 1000 / 2000 USD per 1M (the flat price of `common::price`), and
/// 3000 / 4000 above `above` input tokens.
fn tiered(mode: TierMode, above: u64) -> ModelPricing {
    let mut p = price("p", "m");
    p.tiering = Some(Tiering {
        mode,
        tiers: vec![PriceTier {
            above_input_tokens: above,
            input_per_million: d("3000"),
            output_per_million: d("4000"),
            cached_input_per_million: None,
            cache_write_per_million: None,
            reasoning_per_million: None,
        }],
    });
    p
}

/// `common::price` with every rate doubled (a fixed sheet: one version).
fn doubled() -> ModelPricing {
    let mut p = ModelPricing::per_million(ProviderId::new("p"), "m", d("2000"), d("4000"));
    p.effective_from = "2026-10-01T00:00:00Z".parse().unwrap();
    p
}

fn ok_with(prompt: u64, completion: u64) -> Scripted {
    Scripted::always("p", Step::Ok(Some(usage(prompt, completion))))
}

fn by_usage(prompt: u64, completion: u64, source: &str) -> Reconciliation {
    Reconciliation::Usage {
        usage: usage(prompt, completion),
        source: source.into(),
    }
}

async fn row(c: &AiClient, id: &RequestId) -> universal_ai::RequestUsage {
    c.get_ai_request(id).await.unwrap().unwrap()
}

// ---------- reserve → settle → release ----------

/// "hi" + max_tokens 100 has an input bound of 18 tokens: a tier above 10 is
/// reachable, so 18 × 3000 + 100 × 4000 per 1M = 0.454 is reserved. The
/// provider reports 10 input tokens — exactly the threshold, the base tier —
/// so 0.02 is charged and the rest is released.
#[tokio::test]
async fn reservation_covers_the_reachable_tier_and_settlement_releases_the_rest() {
    let c = client(vec![ok_with(10, 5)], ClientOpts::default());
    let sheet = tiered(TierMode::WholeRequest, 10);
    c.pricing().try_upsert(sheet.clone()).unwrap();

    let mut request = ChatRequest::simple("m", "hi");
    request.max_tokens = Some(100);
    let worst = c.cost().worst_case(&request).await.unwrap();
    assert_eq!(worst.total, d("0.454"));
    assert!(
        worst.explain().contains("exceeds 10"),
        "{}",
        worst.explain()
    );

    let r = send(&c).await.unwrap();
    let a = row(&c, &r.request_id).await.accounting;
    assert_eq!(a.reserved_cost, Some(d("0.454")));
    assert_eq!(a.status, CostStatus::Actual);
    assert_eq!(a.charged_cost, Some(d("0.02")));
    assert_eq!(a.pricing_version, Some(sheet.version()));
    assert_eq!(r.cost.unwrap().tier_above_input_tokens, None);
    assert_eq!(
        spent_today(&c).await,
        d("0.02"),
        "released to the actual cost"
    );
}

#[tokio::test]
async fn usage_above_the_threshold_is_charged_at_the_tier() {
    // WholeRequest: 11 × 3000 + 5 × 4000 per 1M.
    let c = client(vec![ok_with(11, 5)], ClientOpts::default());
    c.pricing()
        .try_upsert(tiered(TierMode::WholeRequest, 10))
        .unwrap();
    let r = send(&c).await.unwrap();
    let cost = r.cost.unwrap();
    assert_eq!(cost.amount, d("0.053"));
    assert_eq!(cost.tier_above_input_tokens, Some(10));
    assert_eq!(spent_today(&c).await, d("0.053"));

    // Marginal: 10 × 1000 + 5 × 3000 + 5 × 4000 (output at the reached tier).
    let c = client(vec![ok_with(15, 5)], ClientOpts::default());
    c.pricing()
        .try_upsert(tiered(TierMode::Marginal, 10))
        .unwrap();
    let r = send(&c).await.unwrap();
    assert_eq!(r.cost.unwrap().amount, d("0.045"));
    assert_eq!(spent_today(&c).await, d("0.045"));
}

#[tokio::test]
async fn an_unreachable_tier_does_not_raise_the_reservation() {
    // Input bound 18 <= 20: only the base rates can apply.
    let c = client(vec![ok_with(10, 5)], ClientOpts::default());
    c.pricing()
        .try_upsert(tiered(TierMode::WholeRequest, 20))
        .unwrap();
    let r = send(&c).await.unwrap();
    let a = row(&c, &r.request_id).await.accounting;
    assert_eq!(a.reserved_cost, Some(d(WORST)));
    assert_eq!(a.charged_cost, Some(d(ACTUAL)));
}

#[tokio::test]
async fn a_tighter_budget_refuses_the_expensive_tier_before_dispatch() {
    let p = ok_with(10, 5);
    let c = client(
        vec![p.clone()],
        ClientOpts {
            budget: universal_ai::BudgetPolicy::daily_usd(d("0.3")),
            ..Default::default()
        },
    );
    c.pricing()
        .try_upsert(tiered(TierMode::WholeRequest, 10))
        .unwrap();
    // The flat worst case (0.218) would fit; the reachable tier (0.454) does not.
    let err = send(&c).await.unwrap_err();
    match err {
        universal_ai::AiError::DailyLimitExceeded { requested, .. } => {
            assert_eq!(requested, d("0.454"))
        }
        other => panic!("expected DailyLimitExceeded, got {other:?}"),
    }
    assert_eq!(p.calls(), 0, "nothing sent");
    assert_eq!(spent_today(&c).await, Decimal::ZERO);
}

// ---------- price changes ----------

#[tokio::test]
async fn a_price_change_mid_flight_does_not_reprice_the_attempt() {
    let c = Arc::new(client(
        vec![Scripted::always(
            "p",
            Step::Delayed(Duration::from_millis(300), Some(usage(10, 5))),
        )],
        ClientOpts::default(),
    ));
    let reserved_with = c
        .pricing()
        .get_price_sync(&ProviderId::new("p"), &model())
        .unwrap();
    let in_flight = tokio::spawn({
        let c = Arc::clone(&c);
        async move { send(&c).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    c.pricing().try_upsert(doubled()).unwrap();

    let r = in_flight.await.unwrap().unwrap();
    let a = row(&c, &r.request_id).await.accounting;
    assert_eq!(a.charged_cost, Some(d(ACTUAL)), "priced as reserved");
    assert_eq!(a.pricing_version, Some(reserved_with.version()));

    // The next attempt uses the new sheet.
    let r = send(&c).await.unwrap();
    let a = row(&c, &r.request_id).await.accounting;
    assert_eq!(a.charged_cost, Some(d("0.04")));
    assert_eq!(a.pricing_version, Some(doubled().version()));
}

#[tokio::test]
async fn reconciliation_reprices_with_the_attempts_own_sheet() {
    let c = client(vec![ok_with(10, 5)], ClientOpts::default());
    let r = send(&c).await.unwrap();
    let version = row(&c, &r.request_id)
        .await
        .accounting
        .pricing_version
        .unwrap();
    c.pricing().try_upsert(doubled()).unwrap();

    // The provider's export says 20 / 10: at the attempt's rates 0.04, not 0.08.
    let fixed = c
        .reconcile_attempt(&r.request_id, by_usage(20, 10, "usage export"))
        .await
        .unwrap();
    assert_eq!(fixed.accounting.status, CostStatus::Reconciled);
    assert_eq!(fixed.accounting.charged_cost, Some(d("0.04")));
    assert_eq!(fixed.accounting.pricing_version.as_deref(), Some(&*version));
    let note = fixed.accounting.cost_note.unwrap();
    assert!(note.contains(&format!("price {version}")), "{note}");
    assert_eq!(spent_today(&c).await, d("0.04"));
}

// ---------- persistence ----------

#[tokio::test]
async fn price_sheets_survive_a_restart() {
    let db = temp("restart.db");
    let (id, sheet) = {
        let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
        let c = client(
            vec![ok_with(11, 5)],
            ClientOpts {
                storage: Some(storage.clone()),
                ..Default::default()
            },
        );
        let sheet = tiered(TierMode::WholeRequest, 10);
        c.pricing().try_upsert(sheet.clone()).unwrap();
        let r = send(&c).await.unwrap();
        // Stored before (and with) the row that references it.
        assert_eq!(
            storage.get_pricing_version(&sheet.version()).await.unwrap(),
            Some(sheet.clone())
        );
        (r.request_id, sheet)
    };

    // A new process with a different current price.
    let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    let c = client(
        vec![ok_with(10, 5)],
        ClientOpts {
            storage: Some(storage.clone()),
            ..Default::default()
        },
    );
    c.pricing().try_upsert(doubled()).unwrap();
    let before = row(&c, &id).await;
    assert_eq!(
        before.accounting.pricing_version,
        Some(sheet.version()),
        "read back"
    );
    assert_eq!(before.cost.unwrap().tier_above_input_tokens, Some(10));
    assert_eq!(spent_today(&c).await, d("0.053"));

    // Reconciled at the stored tiered sheet: 12 × 3000 + 5 × 4000 per 1M.
    let fixed = c
        .reconcile_attempt(&id, by_usage(12, 5, "usage export"))
        .await
        .unwrap();
    assert_eq!(fixed.accounting.charged_cost, Some(d("0.056")));
    assert_eq!(spent_today(&c).await, d("0.056"));
    drop(c);
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
async fn a_tampered_price_sheet_is_refused() {
    let db = temp("tamper.db");
    let storage = SqliteStorage::connect(&db_url(&db)).await.unwrap();
    let sheet = price("p", "m");
    storage.save_pricing_version(&sheet).await.unwrap();
    storage.save_pricing_version(&sheet).await.unwrap(); // idempotent
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url(&db))
        .await
        .unwrap();
    sqlx::query("UPDATE price_versions SET pricing_json = replace(pricing_json, '2000', '20')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let err = storage
        .get_pricing_version(&sheet.version())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("corrupt price version"), "{err}");
    assert_eq!(storage.get_pricing_version("pv1-none").await.unwrap(), None);
    let _ = std::fs::remove_file(db);
}

/// A schema v3 database (no `price_versions`, no `pricing_version` column) is
/// migrated in place: its rows and totals are unchanged, its attempts have no
/// recorded sheet — reconciling one uses the current sheet and says so — and new
/// attempts record theirs.
#[tokio::test]
async fn schema_v3_database_is_migrated() {
    let db = temp("v3.db");
    let old_id = {
        let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
        let c = client(
            vec![ok_with(10, 5)],
            ClientOpts {
                storage: Some(storage),
                ..Default::default()
            },
        );
        send(&c).await.unwrap().request_id
    };
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url(&db))
        .await
        .unwrap();
    for sql in [
        "DROP TABLE price_versions",
        "ALTER TABLE requests DROP COLUMN pricing_version",
        "PRAGMA user_version = 3",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let storage = Arc::new(SqliteStorage::connect(&db_url(&db)).await.unwrap());
    assert_eq!(
        storage.schema_version().await.unwrap(),
        universal_ai::storage::SQLITE_SCHEMA_VERSION
    );
    let c = client(
        vec![ok_with(10, 5)],
        ClientOpts {
            storage: Some(storage.clone()),
            ..Default::default()
        },
    );
    c.pricing().try_upsert(doubled()).unwrap();
    let old = row(&c, &old_id).await;
    assert_eq!(old.accounting.pricing_version, None);
    assert_eq!(old.accounting.charged_cost, Some(d(ACTUAL)));
    assert_eq!(spent_today(&c).await, d(ACTUAL), "totals unchanged");

    let fixed = c
        .reconcile_attempt(&old_id, by_usage(10, 5, "usage export"))
        .await
        .unwrap();
    assert_eq!(fixed.accounting.charged_cost, Some(d("0.04")));
    let note = fixed.accounting.cost_note.clone().unwrap();
    assert!(
        note.contains("current: the attempt had no recorded price"),
        "{note}"
    );
    assert_eq!(fixed.accounting.pricing_version, Some(doubled().version()));
    assert_eq!(
        storage
            .get_pricing_version(&doubled().version())
            .await
            .unwrap(),
        Some(doubled())
    );

    let r = send(&c).await.unwrap();
    assert_eq!(
        row(&c, &r.request_id).await.accounting.pricing_version,
        Some(doubled().version())
    );
    assert_eq!(spent_today(&c).await, d("0.08"));
    drop(c);

    // An older binary must not open it.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url(&db))
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version = 5")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(SqliteStorage::connect(&db_url(&db)).await.is_err());
    let _ = std::fs::remove_file(db);
}
