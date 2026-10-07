//! Fail-closed budget control and cost accounting (mock HTTP, fake keys).
//!
//! Prices: input $1000/M ($0.001/token), output $2000/M ($0.002/token).
//! `message("hi")` → input upper bound 18 tokens (2 bytes + overheads), so with
//! `max_tokens(100)` the worst case is 0.018 + 0.200 = **0.218**; the mock's actual
//! usage (10 in / 5 out) costs **0.02**.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, TimeZone, Utc};
use futures::StreamExt;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::{json, Value};
use universal_ai::http::{HttpClient, HttpConfig};
use universal_ai::{
    AccountId, AddKeyRequest, AiClient, AiConfig, AiError, BudgetPolicy, CostAccounting,
    CostStatus, KeySelectionStrategy, MemoryStorage, MissingUsagePolicy, ModelCapabilities,
    ModelId, ModelInfo, ModelPricing, OpenAICompatible, ProviderId, RequestId, RequestUsage,
    RetryPolicy, SqliteStorage, Storage, StreamEvent, Usage,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

const WORST: &str = "0.218";
const ACTUAL: &str = "0.02";

fn reply(usage: Option<(u64, u64)>, text: &str) -> ResponseTemplate {
    let mut body = json!({
        "choices": [{ "message": { "role": "assistant", "content": text },
                      "finish_reason": "stop" }]
    });
    if let Some((p, c)) = usage {
        body["usage"] =
            json!({ "prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c });
    }
    ResponseTemplate::new(200).set_body_json(body)
}

async fn mock(
    responder: impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder)
        .mount(&server)
        .await;
    server
}

fn price(provider: ProviderId, model: &str) -> ModelPricing {
    ModelPricing {
        provider,
        model: ModelId::new(model),
        input_per_million: Some(d("1000")),
        output_per_million: Some(d("2000")),
        cached_input_per_million: None,
        cache_write_per_million: None,
        reasoning_per_million: None,
        effective_from: Utc::now(),
        tiering: None,
    }
}

fn provider(server: &MockServer, id: &str) -> OpenAICompatible {
    OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("KEY_A_TEST".into()))
        .provider_id(ProviderId::new(id))
        .build()
        .unwrap()
}

fn config(budget: BudgetPolicy) -> AiConfig {
    AiConfig {
        budget,
        retry_policy: RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        },
        ..AiConfig::default()
    }
}

fn daily(limit: &str) -> BudgetPolicy {
    BudgetPolicy::daily_usd(d(limit))
}

/// Client with one priced OpenAI-compatible provider (model "m").
fn client(
    server: &MockServer,
    budget: BudgetPolicy,
    storage: Option<Arc<dyn Storage>>,
) -> AiClient {
    let mut builder = AiClient::builder()
        .provider(provider(server, "openai-compatible"))
        .config(config(budget));
    if let Some(storage) = storage {
        builder = builder.storage(storage);
    }
    let client = builder.build().unwrap();
    client
        .pricing()
        .upsert(price(ProviderId::openai_compatible(), "m"));
    client
}

async fn send(client: &AiClient) -> Result<universal_ai::ChatResponse, AiError> {
    client
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .send()
        .await
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

async fn spent_today(client: &AiClient) -> Decimal {
    client.budget_status(None).await.unwrap().daily_spent
}

fn row(client: &AiClient, id: &RequestId) -> RequestUsage {
    client.request_usage(id).expect("accounting row")
}

// ---------- A. unknown price ----------

#[tokio::test]
async fn a_unknown_price_blocks_budgeted_request_before_sending() {
    let server = mock(|_| reply(Some((10, 5)), "x")).await;
    let client = client(&server, daily("10"), None);

    let err = client
        .chat()
        .model("unpriced")
        .message("hi")
        .max_tokens(100)
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::PricingUnavailable { .. }), "{err:?}");
    assert!(err.to_string().contains("pricing is unavailable"));
    assert_eq!(hits(&server).await, 0, "nothing may reach the provider");

    let rows = client.list_ai_requests(5).await.unwrap();
    assert_eq!(rows[0].accounting.status, CostStatus::Rejected);
    assert!(rows[0]
        .accounting
        .rejection
        .as_deref()
        .unwrap()
        .contains("pricing"));
    assert_eq!(spent_today(&client).await, Decimal::ZERO);
}

#[tokio::test]
async fn unknown_price_without_budget_is_allowed_but_not_zero() {
    let server = mock(|_| reply(Some((10, 5)), "x")).await;
    let client = client(&server, BudgetPolicy::default(), None);

    let response = client
        .chat()
        .model("unpriced")
        .message("hi")
        .send()
        .await
        .unwrap();
    assert!(response.cost.is_none(), "unknown cost must not become 0");
    let r = row(&client, &response.request_id);
    assert_eq!(r.accounting.status, CostStatus::PricingUnavailable);
    assert_eq!(r.accounting.charged_cost, None);
    let stats = client.stats().all().await;
    assert_eq!(stats.unknown_cost_requests, 1);
    assert_eq!(stats.total_cost, Decimal::ZERO);
}

// ---------- B / J. known price, actual vs estimated ----------

#[tokio::test]
async fn b_known_price_actual_cost_and_released_reservation() {
    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let client = client(&server, daily("10"), None);

    let response = send(&client).await.unwrap();
    assert_eq!(response.cost.as_ref().unwrap().amount, d(ACTUAL));
    let r = row(&client, &response.request_id);
    assert_eq!(r.accounting.status, CostStatus::Actual);
    assert_eq!(r.accounting.estimated_cost, Some(d(WORST)));
    assert_eq!(r.accounting.charged_cost, Some(d(ACTUAL)));
    // J: the unused part of the reservation (0.218 - 0.02) is released.
    assert_eq!(spent_today(&client).await, d(ACTUAL));
}

// ---------- C. max_tokens in the pre-flight estimate ----------

#[tokio::test]
async fn c_max_tokens_bounds_the_worst_case() {
    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let client = client(
        &server,
        BudgetPolicy {
            max_request_cost: Some(d("0.1")),
            ..Default::default()
        },
        None,
    );

    // 100 output tokens → worst case 0.218 > 0.1.
    let err = send(&client).await.unwrap_err();
    assert!(matches!(err, AiError::BudgetExceeded { .. }), "{err:?}");
    // 10 output tokens → 0.018 + 0.02 = 0.038 fits.
    client
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(10)
        .send()
        .await
        .unwrap();
    assert_eq!(hits(&server).await, 1);

    // No max_tokens and no known model limit: the cost cannot be bounded.
    let err = client
        .chat()
        .model("m")
        .message("hi")
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::OutputLimitUnknown { .. }), "{err:?}");
    assert_eq!(hits(&server).await, 1);

    // A registered output limit is used as the bound instead.
    client.models().register(ModelInfo {
        id: ModelId::new("m"),
        provider: ProviderId::openai_compatible(),
        name: None,
        context_window: None,
        max_output_tokens: Some(20),
        capabilities: ModelCapabilities::chat_default(),
        pricing: None,
    });
    let response = client.chat().model("m").message("hi").send().await.unwrap();
    assert_eq!(
        row(&client, &response.request_id).accounting.estimated_cost,
        Some(d("0.058"))
    );
}

#[tokio::test]
async fn d_per_request_cap_blocks_before_sending() {
    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let client = client(&server, BudgetPolicy::default(), None);
    let err = client
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .max_cost(d("0.05"))
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::BudgetExceeded { .. }), "{err:?}");
    assert!(err.to_string().contains("remaining budget"));
    assert_eq!(hits(&server).await, 0);
}

// ---------- F. daily limit ----------

#[tokio::test]
async fn f_daily_limit_blocks_next_request() {
    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let client = client(&server, daily("0.25"), None);

    send(&client).await.unwrap(); // 0 + 0.218 ≤ 0.25 → spends 0.02
    send(&client).await.unwrap(); // 0.02 + 0.218 ≤ 0.25 (only because 0.198 was released)
    let err = send(&client).await.unwrap_err(); // 0.04 + 0.218 > 0.25
    match err {
        AiError::DailyLimitExceeded {
            scope,
            spent,
            requested,
            limit,
        } => {
            assert_eq!(scope, None);
            assert_eq!(spent, d("0.04"));
            assert_eq!(requested, d(WORST));
            assert_eq!(limit, d("0.25"));
        }
        other => panic!("expected daily limit, got {other:?}"),
    }
    assert_eq!(hits(&server).await, 2);
}

// ---------- G. monthly limit + UTC rollover ----------

fn seeded(at: chrono::DateTime<Utc>, charged: &str) -> RequestUsage {
    RequestUsage {
        request_id: RequestId::new(),
        provider: ProviderId::openai_compatible(),
        account: AccountId::new("default"),
        api_key: None,
        model: ModelId::new("m"),
        started_at: at,
        finished_at: at,
        usage: Usage::default(),
        cost: None,
        success: true,
        latency_ms: 1,
        request_json: json!({}),
        response_json: None,
        importance: None,
        accounting: CostAccounting {
            status: CostStatus::Actual,
            charged_cost: Some(d(charged)),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn g_monthly_limit_and_period_rollover() {
    let now = Utc::now();
    let month_start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap();
    let storage = Arc::new(MemoryStorage::new());
    // Last second of the previous month and yesterday: outside the current periods.
    storage
        .save_request(&seeded(month_start - chrono::Duration::seconds(1), "100"))
        .await
        .unwrap();
    // Earlier this month.
    storage
        .save_request(&seeded(month_start, "0.2"))
        .await
        .unwrap();

    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let monthly = client(
        &server,
        BudgetPolicy {
            max_monthly_cost: Some(d("0.3")),
            ..Default::default()
        },
        Some(storage.clone()),
    );
    let status = monthly.budget_status(None).await.unwrap();
    assert_eq!(
        status.monthly_spent,
        d("0.2"),
        "previous month must not count"
    );
    let err = send(&monthly).await.unwrap_err(); // 0.2 + 0.218 > 0.3
    assert!(
        matches!(err, AiError::MonthlyLimitExceeded { .. }),
        "{err:?}"
    );
    assert_eq!(hits(&server).await, 0);

    // Daily rollover: yesterday's spend does not count today.
    let storage = Arc::new(MemoryStorage::new());
    storage
        .save_request(&seeded(now - chrono::Duration::days(1), "100"))
        .await
        .unwrap();
    let daily_client = client(&server, daily("0.25"), Some(storage));
    send(&daily_client).await.unwrap();
    assert_eq!(spent_today(&daily_client).await, d(ACTUAL));
}

// ---------- H. persistence across restart ----------

#[tokio::test]
async fn h_spend_survives_restart() {
    let server = mock(|_| reply(Some((10, 5)), "ok")).await;
    let file = std::env::temp_dir().join(format!("budget-{}.db", RequestId::new()));
    let url = format!("sqlite://{}?mode=rwc", file.display());

    let before = {
        let storage = Arc::new(SqliteStorage::connect(&url).await.unwrap());
        let client = client(&server, daily("0.25"), Some(storage));
        send(&client).await.unwrap();
        send(&client).await.unwrap();
        client.budget_status(None).await.unwrap()
    };
    assert_eq!(before.daily_spent, d("0.04"));

    // "Restart": fresh storage handle + client on the same database.
    let storage = Arc::new(SqliteStorage::connect(&url).await.unwrap());
    let client = client(&server, daily("0.25"), Some(storage));
    let after = client.budget_status(None).await.unwrap();
    assert_eq!(after, before);
    let err = send(&client).await.unwrap_err();
    assert!(matches!(err, AiError::DailyLimitExceeded { .. }), "{err:?}");
    assert_eq!(hits(&server).await, 2);
    let _ = std::fs::remove_file(file);
}

// ---------- I. concurrent reservations ----------

#[tokio::test]
async fn i_concurrent_requests_cannot_overspend() {
    let server = mock(|_| reply(Some((10, 5)), "ok").set_delay(Duration::from_millis(200))).await;
    let client = client(&server, daily("0.5"), None);

    // Each worst case is 0.218: only two fit in 0.5 while all are in flight.
    let results = futures::future::join_all((0..6).map(|_| send(&client))).await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let limited = results
        .iter()
        .filter(|r| matches!(r, Err(AiError::DailyLimitExceeded { .. })))
        .count();
    assert_eq!((ok, limited), (2, 4));
    assert_eq!(hits(&server).await, 2);
    assert_eq!(spent_today(&client).await, d("0.04"));
}

// ---------- K. unknown usage ----------

#[tokio::test]
async fn k_missing_usage_keeps_the_reservation() {
    let server = mock(|_| reply(None, "no usage here")).await;
    let client = client(&server, daily("1"), None);

    let response = send(&client).await.unwrap();
    assert!(response.cost.is_none());
    let r = row(&client, &response.request_id);
    assert_eq!(r.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(r.accounting.charged_cost, Some(d(WORST)), "unknown ≠ 0");
    assert_eq!(spent_today(&client).await, d(WORST));

    // Policy may also reject such responses (still charged).
    let strict = client_with_missing_usage_reject(&server);
    let err = send(&strict).await.unwrap_err();
    assert!(matches!(err, AiError::UsageUnavailable { .. }), "{err:?}");
    assert_eq!(spent_today(&strict).await, d(WORST));

    // Without any budget nothing is reserved, but the cost is still not zero:
    // the attempt is charged its worst-case estimate.
    let open = client_no_budget(&server);
    let response = send(&open).await.unwrap();
    let r = row(&open, &response.request_id);
    assert_eq!(r.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(r.accounting.reserved_cost, None);
    assert_eq!(r.accounting.charged_cost, Some(d(WORST)));
    assert_eq!(open.stats().all().await.unknown_cost_requests, 1);
}

fn client_with_missing_usage_reject(server: &MockServer) -> AiClient {
    client(
        server,
        BudgetPolicy {
            max_daily_cost: Some(d("1")),
            missing_usage: MissingUsagePolicy::Reject,
            ..Default::default()
        },
        None,
    )
}

fn client_no_budget(server: &MockServer) -> AiClient {
    client(server, BudgetPolicy::default(), None)
}

// ---------- L. streaming ----------

const SSE_WITH_USAGE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}],\"usage\":null}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}],\"usage\":null}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
data: [DONE]\n\n";

const SSE_NO_USAGE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
data: [DONE]\n\n";

fn sse(body: &'static str) -> impl Fn(&Request) -> ResponseTemplate {
    move |_| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

async fn drain(client: &AiClient) -> Result<(String, Option<Usage>), AiError> {
    let mut stream = client
        .chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .stream()
        .await?;
    let (mut text, mut usage) = (String::new(), None);
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::Usage { usage: u } => usage = Some(u),
            _ => {}
        }
    }
    Ok((text, usage))
}

#[tokio::test]
async fn l_streaming_is_budgeted_and_metered() {
    let server = mock(sse(SSE_WITH_USAGE)).await;
    let client = client(&server, daily("0.25"), None);

    let (text, usage) = drain(&client).await.unwrap();
    assert_eq!(text, "Hi!");
    assert_eq!(
        usage.unwrap().total_tokens,
        15,
        "`usage: null` chunks are ignored"
    );
    let body: Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body["stream_options"]["include_usage"], true);

    let r = &client.list_ai_requests(1).await.unwrap()[0];
    assert!(r.success);
    assert_eq!(r.usage.prompt_tokens, 10);
    assert_eq!(r.accounting.status, CostStatus::Actual);
    assert_eq!(r.cost.as_ref().unwrap().amount, d(ACTUAL));
    assert_eq!(spent_today(&client).await, d(ACTUAL));
    assert_eq!(client.stats().all().await.total_tokens, 15);

    // Budget gate applies before a stream is opened.
    drain(&client).await.unwrap(); // 0.02 + 0.218 ≤ 0.25
    let err = drain(&client).await.unwrap_err();
    assert!(matches!(err, AiError::DailyLimitExceeded { .. }), "{err:?}");
    assert_eq!(hits(&server).await, 2);
}

#[tokio::test]
async fn streaming_without_usage_keeps_reservation() {
    let server = mock(sse(SSE_NO_USAGE)).await;
    let client = client(&server, daily("1"), None);
    drain(&client).await.unwrap();
    let r = &client.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(r.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(r.accounting.charged_cost, Some(d(WORST)));
    assert_eq!(spent_today(&client).await, d(WORST));
}

// ---------- M. provider fallback ----------

async fn fallback_client(alpha: &MockServer, beta: &MockServer, timeout_ms: u64) -> AiClient {
    // Providers built directly use their own HTTP client: give them the timeout.
    let http = HttpClient::new(HttpConfig {
        request_timeout: Duration::from_millis(timeout_ms),
        ..HttpConfig::default()
    })
    .unwrap();
    let with_timeout = |server: &MockServer, id: &str| {
        OpenAICompatible::builder()
            .base_url(format!("{}/v1", server.uri()))
            .api_key(SecretString::new("KEY_A_TEST".into()))
            .provider_id(ProviderId::new(id))
            .http(http.clone())
            .build()
            .unwrap()
    };
    let client = AiClient::builder()
        .provider(with_timeout(alpha, "alpha"))
        .provider(with_timeout(beta, "beta"))
        .config(AiConfig {
            fallback: true,
            ..config(daily("1"))
        })
        .build()
        .unwrap();
    client
        .pricing()
        .upsert(price(ProviderId::new("alpha"), "m"));
    client.pricing().upsert(price(ProviderId::new("beta"), "m"));
    client
}

#[tokio::test]
async fn m_fallback_charges_only_the_attempt_that_consumed_tokens() {
    let alpha = mock(|_| ResponseTemplate::new(500).set_body_string("down")).await;
    let beta = mock(|_| reply(Some((10, 5)), "from beta")).await;
    let client = fallback_client(&alpha, &beta, 5_000).await;

    let response = send(&client).await.unwrap();
    assert_eq!(response.text(), "from beta");
    let rows = client.list_ai_requests(10).await.unwrap();
    assert_eq!(rows.len(), 2, "one row per provider attempt");
    let (a, b) = if rows[0].provider.as_str() == "alpha" {
        (&rows[0], &rows[1])
    } else {
        (&rows[1], &rows[0])
    };
    assert_eq!(a.accounting.status, CostStatus::NotCharged);
    assert_eq!(a.accounting.charged_cost, Some(Decimal::ZERO));
    assert_eq!(b.accounting.status, CostStatus::Actual);
    assert_eq!(b.accounting.charged_cost, Some(d(ACTUAL)));
    assert_eq!(
        a.accounting.logical_request_id,
        b.accounting.logical_request_id
    );
    assert_eq!((a.accounting.attempt, b.accounting.attempt), (1, 2));
    assert_eq!(b.request_id, response.request_id);
    assert_eq!(
        spent_today(&client).await,
        d(ACTUAL),
        "no double / fake charge"
    );
}

#[tokio::test]
async fn fallback_after_timeout_keeps_the_ambiguous_charge() {
    // A timed-out request may still be billed: its reservation stays charged.
    let alpha = mock(|_| reply(Some((10, 5)), "late").set_delay(Duration::from_secs(3))).await;
    let beta = mock(|_| reply(Some((10, 5)), "from beta")).await;
    let client = fallback_client(&alpha, &beta, 300).await;

    send(&client).await.unwrap();
    let total: Decimal = d(WORST) + d(ACTUAL);
    assert_eq!(spent_today(&client).await, total);
    let rows = client.list_ai_requests(10).await.unwrap();
    let a = rows
        .iter()
        .find(|r| r.provider.as_str() == "alpha")
        .unwrap();
    assert_eq!(a.accounting.status, CostStatus::UsageUnavailable);
}

// ---------- N. key accounting ----------

#[tokio::test]
async fn n_cost_is_attributed_to_the_key_that_was_used() {
    let server = mock(|req| {
        let key = req
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches("Bearer ")
            .to_string();
        // Answer with a label, so the secret itself never enters response text.
        let label = if key == "KEY_A_TEST" {
            "label-a"
        } else {
            "label-b"
        };
        reply(Some((10, 5)), label)
    })
    .await;
    let client = AiClient::builder()
        .allow_empty_providers()
        .config(config(daily("1")))
        .build()
        .unwrap();
    client
        .pricing()
        .upsert(price(ProviderId::openai_compatible(), "m"));
    client
        .keys()
        .set_selection_strategy(KeySelectionStrategy::RoundRobin)
        .await;
    let mut ids = std::collections::HashMap::new();
    for secret in ["KEY_A_TEST", "KEY_B_TEST"] {
        let info = client
            .keys()
            .add_key(AddKeyRequest {
                provider: ProviderId::openai_compatible(),
                account_id: AccountId::new("acct"),
                secret: SecretString::new(secret.into()),
                name: None,
                base_url: Some(format!("{}/v1", server.uri())),
            })
            .await
            .unwrap();
        client
            .sync_provider_from_key(&info, info.base_url.as_deref())
            .await
            .unwrap();
        let label = if secret == "KEY_A_TEST" {
            "label-a"
        } else {
            "label-b"
        };
        ids.insert(label.to_string(), info.id);
    }

    for _ in 0..4 {
        let response = send(&client).await.unwrap();
        let r = row(&client, &response.request_id);
        assert_eq!(r.api_key.as_ref(), ids.get(&response.text()));
        assert_eq!(r.accounting.charged_cost, Some(d(ACTUAL)));
    }
    for id in ids.values() {
        let info = client.keys().get_key(id).await.unwrap();
        assert_eq!(info.usage.requests, 2);
        assert_eq!(info.usage.total_cost, d("0.04"));
    }
    let persisted = serde_json::to_string(&client.list_ai_requests(10).await.unwrap()).unwrap();
    assert!(!persisted.contains("KEY_A_TEST") && !persisted.contains("KEY_B_TEST"));
}

/// `require_cost_bound` turns a request without any limit into a budget-controlled
/// one: unknown pricing is refused before HTTP and a response without usage is
/// charged its reservation instead of nothing.
#[tokio::test]
async fn require_cost_bound_refuses_unpriced_and_never_charges_nothing() {
    let server = mock(|_| reply(None, "ok")).await;
    let c = client_no_budget(&server); // "m" is priced, "unpriced" is not

    let err = c
        .chat()
        .model("unpriced")
        .message("hi")
        .max_tokens(100)
        .require_cost_bound()
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::PricingUnavailable { .. }), "{err:?}");
    assert_eq!(hits(&server).await, 0, "refused before HTTP");

    c.chat()
        .model("m")
        .message("hi")
        .max_tokens(100)
        .require_cost_bound()
        .send()
        .await
        .unwrap();
    let row = &c.list_ai_requests(1).await.unwrap()[0];
    assert_eq!(row.accounting.status, CostStatus::UsageUnavailable);
    assert_eq!(row.accounting.reserved_cost, Some(d(WORST)));
    assert_eq!(
        row.accounting.charged_cost,
        Some(d(WORST)),
        "no usage: the reservation is charged, not zero"
    );
}
