//! Property tests (deterministic generator, thousands of cases):
//! cost >= 0, worst-case estimate >= actual cost for every usage within the
//! bounds, unknown never silently priced, settlement writes are idempotent.

mod common;

use chrono::Utc;
use common::Rng;
use rust_decimal::Decimal;
use universal_ai::{
    AccountId, CostAccounting, CostManager, CostStatus, MemoryStorage, ModelId, ModelPricing,
    PricingRegistry, ProviderId, RequestId, RequestUsage, Storage, Usage,
};

fn rate(rng: &mut Rng) -> Decimal {
    Decimal::new(rng.below(50_000) as i64, 2) // 0 ..= 499.99 per M
}

fn opt_rate(rng: &mut Rng) -> Option<Decimal> {
    (rng.below(2) == 0).then(|| rate(rng))
}

fn pricing(rng: &mut Rng) -> ModelPricing {
    let mut p = ModelPricing::per_million(ProviderId::openai(), "m", rate(rng), rate(rng));
    p.cached_input_per_million = opt_rate(rng);
    p.cache_write_per_million = opt_rate(rng);
    p.reasoning_per_million = opt_rate(rng);
    p
}

/// Usage consistent with an input bound and an output bound.
fn usage_within(rng: &mut Rng, input_bound: u64, output_bound: u64) -> Usage {
    let prompt = rng.below(input_bound + 1);
    let cached = rng.below(prompt + 1);
    let creation = rng.below(prompt - cached + 1);
    let completion = rng.below(output_bound + 1);
    let reasoning = rng.below(completion + 1);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: Some(cached),
        cache_creation_tokens: Some(creation),
        reasoning_tokens: Some(reasoning),
        ..Default::default()
    }
}

#[test]
fn estimate_dominates_any_cost_within_bounds_and_cost_is_non_negative() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    let mut priced = 0;
    for _ in 0..5_000 {
        let p = pricing(&mut rng);
        let reg = PricingRegistry::new();
        reg.upsert(p.clone());
        let mgr = CostManager::new(reg);
        let input_bound = 1 + rng.below(100_000);
        let output_bound = 1 + rng.below(100_000);
        let estimate = mgr
            .estimate(
                &ProviderId::openai(),
                &ModelId::new("m"),
                input_bound,
                Some(output_bound),
            )
            .unwrap()
            .expect("input and output rates exist");
        assert!(estimate.total >= Decimal::ZERO);
        for _ in 0..4 {
            let u = usage_within(&mut rng, input_bound, output_bound);
            match mgr.price(&ProviderId::openai(), &ModelId::new("m"), &u) {
                Ok(cost) => {
                    priced += 1;
                    assert!(cost.amount >= Decimal::ZERO);
                    assert!(
                        cost.amount <= estimate.total,
                        "cost {} > estimate {} for {u:?} with {p:?}",
                        cost.amount,
                        estimate.total
                    );
                }
                // Only a missing cache-write rate may make in-bound usage unknown.
                Err(gap) => {
                    assert!(p.cache_write_per_million.is_none(), "{gap}");
                    assert!(u.cache_creation_tokens.unwrap_or(0) > 0);
                }
            }
        }
    }
    assert!(priced > 10_000, "generator covers priced cases");
}

#[test]
fn inconsistent_or_uncategorized_usage_is_never_priced() {
    let mut rng = Rng(42);
    let reg = PricingRegistry::new();
    let mut p = ModelPricing::per_million(ProviderId::openai(), "m", Decimal::ONE, Decimal::ONE);
    p.cache_write_per_million = Some(Decimal::ONE);
    reg.upsert(p);
    let mgr = CostManager::new(reg);
    for _ in 0..2_000 {
        let prompt = rng.below(1_000);
        let mut u = Usage {
            prompt_tokens: prompt,
            completion_tokens: rng.below(1_000),
            cached_tokens: Some(prompt + 1 + rng.below(10)),
            ..Default::default()
        };
        assert!(mgr
            .price(&ProviderId::openai(), &ModelId::new("m"), &u)
            .is_err());
        u.cached_tokens = None;
        u.other_tokens
            .insert(format!("input_cat{}", rng.below(5)), 1 + rng.below(9));
        assert!(mgr
            .price(&ProviderId::openai(), &ModelId::new("m"), &u)
            .is_err());
    }
}

#[tokio::test]
async fn settlement_writes_are_idempotent() {
    let storage = MemoryStorage::new();
    let mut rng = Rng(7);
    let mut expected = Decimal::ZERO;
    for _ in 0..200 {
        let charge = Decimal::new(rng.below(10_000) as i64, 4);
        let row = RequestUsage {
            request_id: RequestId::new(),
            provider: ProviderId::openai(),
            account: AccountId::new("a"),
            api_key: None,
            model: ModelId::new("m"),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            usage: Usage::default(),
            cost: None,
            success: true,
            latency_ms: 0,
            request_json: serde_json::json!({}),
            response_json: None,
            importance: None,
            accounting: CostAccounting {
                status: CostStatus::Actual,
                charged_cost: Some(charge),
                ..Default::default()
            },
        };
        // Replaying the same final row (e.g. a retried write) never double counts.
        for _ in 0..=rng.below(3) {
            storage.save_request(&row).await.unwrap();
        }
        expected += charge;
    }
    let day = Utc::now().date_naive();
    let start = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let spent = storage
        .spend_in_window(None, start, start + chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(spent, expected);
}
