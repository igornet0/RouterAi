//! SQLite storage smoke test.

#![cfg(feature = "sqlite")]

use rust_decimal::Decimal;
use universal_ai::{
    Account, AccountId, AccountStatus, Balance, CostAccounting, CostStatus, Currency, ModelId,
    ProviderId, RequestId, RequestUsage, SqliteStorage, Storage, Usage,
};

#[tokio::test]
async fn sqlite_roundtrip() {
    let path = std::env::temp_dir().join(format!("universal-ai-{}.db", RequestId::new()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = SqliteStorage::connect(&url).await.unwrap();

    let account = Account {
        id: AccountId::new("acc-1"),
        provider: ProviderId::deepseek(),
        name: "prod".into(),
        status: AccountStatus::Active,
        credit_budget: None,
        created_at: chrono::Utc::now(),
        last_checked_at: None,
    };
    store.save_account(&account).await.unwrap();

    let balance = Balance {
        provider: ProviderId::deepseek(),
        currency: Currency::usd(),
        total: Decimal::new(4210, 2),
        available: Some(Decimal::new(4210, 2)),
        granted: None,
        topped_up: None,
        updated_at: chrono::Utc::now(),
    };
    store.save_balance(&balance).await.unwrap();

    let request_id = RequestId::new();
    let request_json = serde_json::json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let response_json = serde_json::json!({
        "request_id": request_id.to_string(),
        "model": "deepseek-chat",
        "message": {"role": "assistant", "content": "hi"}
    });
    let row = RequestUsage {
        request_id,
        provider: ProviderId::deepseek(),
        account: account.id.clone(),
        api_key: None,
        model: ModelId::new("deepseek-chat"),
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        },
        cost: None,
        success: true,
        latency_ms: 42,
        request_json: request_json.clone(),
        response_json: Some(response_json.clone()),
        importance: None,
        accounting: CostAccounting {
            status: CostStatus::UsageUnavailable,
            estimated_cost: Some(Decimal::new(42, 3)),
            charged_cost: Some(Decimal::new(42, 3)),
            budget_scope: Some("agent:a1".into()),
            logical_request_id: Some(request_id),
            attempt: 2,
            ..Default::default()
        },
    };
    store.save_request(&row).await.unwrap();

    assert_eq!(store.list_accounts().await.unwrap().len(), 1);
    assert_eq!(store.list_balances().await.unwrap().len(), 1);
    let listed = store.list_requests(10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].request_json, request_json);
    assert_eq!(listed[0].response_json, Some(response_json));
    assert_eq!(listed[0].importance, None);
    assert_eq!(
        listed[0].accounting, row.accounting,
        "accounting survives restart"
    );

    let loaded = store.get_request(&request_id).await.unwrap().unwrap();
    assert_eq!(loaded.request_id, request_id);

    let rated = store.set_importance(&request_id, Some(7)).await.unwrap();
    assert_eq!(rated.importance, Some(7));

    let cleared = store.set_importance(&request_id, None).await.unwrap();
    assert_eq!(cleared.importance, None);

    let bad = store.set_importance(&request_id, Some(11)).await;
    assert!(bad.is_err());

    let _ = std::fs::remove_file(path);
}
