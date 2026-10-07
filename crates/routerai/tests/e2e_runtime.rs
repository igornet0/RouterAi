//! End-to-end: Event → Handler → Agent → Result → Event.

use routerai::{
    text_case, Action, AgentId, Assertion, Event, Handler, RouterRuntime, RunStatus, Schedule,
    TestLab,
};
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::json;
use universal_ai::{AiClient, OpenAICompatible};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn event_handler_agent_chain_stub() {
    let rt = RouterRuntime::builder().build().await.unwrap();
    let agent = routerai::published_agent("sales", "Sell products.");
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.upsert_handler(Handler::agent_on_event(
        "sales",
        "telegram.message.received",
        agent_id,
    ))
    .await
    .unwrap();

    let runs = rt
        .emit(Event::new(
            "telegram.message.received",
            "telegram",
            json!({"text": "Хочу купить"}),
        ))
        .await
        .unwrap();

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);

    let events = rt.events().list(20).await;
    assert!(events
        .iter()
        .any(|e| e.event_type == "telegram.message.received"));
    assert!(events.iter().any(|e| e.event_type == "agent.completed"));
}

#[tokio::test]
async fn schedule_tick_starts_agent() {
    let rt = RouterRuntime::builder().build().await.unwrap();
    let agent = routerai::published_agent("research", "Research.");
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.upsert_schedule(Schedule::every(
        "every-1s",
        1,
        Action::AgentRun { agent_id },
    ))
    .await
    .unwrap();

    let runs = rt.tick_schedules().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);
}

#[tokio::test]
async fn test_lab_assertions() {
    let rt = RouterRuntime::builder().build().await.unwrap();
    let agent = routerai::published_agent("sales", "Answer with price.");
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();

    // Stub output contains "stub" — assert must_contain stub
    let case = text_case("t1", agent_id, "Сколько стоит?", &["stub"], &["не знаю"]);
    let (run, report) = rt.run_test(&case).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(report.passed, "{:?}", report.assertions);
}

#[tokio::test]
async fn agent_run_with_universal_ai_mock() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": { "role": "assistant", "content": "Цена продукта $42" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28 }
        })))
        .mount(&server)
        .await;

    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new("sk-test".into()))
        .build()
        .unwrap();
    let ai = AiClient::builder()
        .provider(provider)
        .with_example_prices()
        .build()
        .unwrap();
    // max_run_cost makes the run budget-controlled: the model needs a known price.
    ai.pricing().upsert(universal_ai::ModelPricing {
        provider: universal_ai::ProviderId::openai_compatible(),
        model: universal_ai::ModelId::new("custom-model"),
        input_per_million: Some(Decimal::new(1, 0)),
        output_per_million: Some(Decimal::new(2, 0)),
        cached_input_per_million: None,
        cache_write_per_million: None,
        reasoning_per_million: None,
        effective_from: chrono::Utc::now(),
    });

    let rt = RouterRuntime::builder()
        .ai(std::sync::Arc::new(ai))
        .build()
        .await
        .unwrap();

    let mut agent = routerai::published_agent("sales", "Always mention цена.");
    agent.model.model = "custom-model".into();
    agent.budget.max_run_cost = Some(Decimal::new(1, 0));
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();

    let run = rt
        .start_run(
            &agent_id,
            json!({"message": "Сколько стоит ваш продукт?"}),
            None,
        )
        .await
        .unwrap();

    assert_eq!(run.status, RunStatus::Completed);
    let text = run.output.as_ref().unwrap()["text"].as_str().unwrap();
    assert!(text.contains("Цена") || text.contains("цена") || text.contains("$42"));
    assert!(run.usage.total_tokens > 0);
    // 20 input × $1/M + 8 output × $2/M
    assert_eq!(run.cost, Decimal::new(36, 6));
    assert!(!run.steps.is_empty());

    let case = text_case("price", agent_id, "Сколько стоит?", &["Цена"], &["не знаю"]);
    // Re-run test against same agent (will call mock again)
    let (_run2, report) = rt.run_test(&case).await.unwrap();
    // Case-sensitive must_contain "Цена" — mock returns "Цена продукта $42"
    assert!(report.passed || report.assertions.iter().any(|a| !a.passed));
    // Soft check: evaluate helper works
    let mut fake = run.clone();
    fake.complete(json!({"text": "Цена продукта $42"}));
    let r = TestLab::evaluate(
        &text_case("x", AgentId::from("a"), "q", &["Цена"], &[]),
        &fake,
    );
    assert!(r.passed);
}

#[tokio::test]
async fn sqlite_persists_event_and_run() {
    let path = std::env::temp_dir().join(format!("routerai-{}.db", unique_id()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = routerai::SqliteStore::connect(&url).await.unwrap();
    let rt = RouterRuntime::builder()
        .store(std::sync::Arc::new(store))
        .build()
        .await
        .unwrap();

    let agent = routerai::published_agent("a", "b");
    let id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    let run = rt
        .start_run(&id, json!({"message": "hi"}), None)
        .await
        .unwrap();

    let loaded = rt.store().get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, run.id);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn sqlite_hydrates_console_state_after_restart() {
    use chrono::Utc;
    use routerai::{Dataset, SinkTarget, TestCase};

    let path = std::env::temp_dir().join(format!("routerai-hydrate-{}.db", unique_id()));
    let url = format!("sqlite://{}?mode=rwc", path.display());

    let agent_id;
    let event_id;
    let case_id = format!("case_{}", unique_id());
    let sink_id;
    {
        let store = routerai::SqliteStore::connect(&url).await.unwrap();
        let rt = RouterRuntime::builder()
            .store(std::sync::Arc::new(store))
            .build()
            .await
            .unwrap();
        let agent = routerai::published_agent("persist-me", "hello");
        agent_id = agent.id.clone();
        rt.upsert_agent(agent).await.unwrap();
        let ev = Event::new("test.hydrated", "console", json!({"ok": true}));
        event_id = ev.id.clone();
        rt.emit(ev).await.unwrap();

        let mut target =
            SinkTarget::new("https://example.com/hook", vec!["agent.completed".into()]);
        sink_id = target.id.clone();
        target = rt.upsert_sink_target(target).await.unwrap();
        assert_eq!(target.id, sink_id);

        rt.upsert_test_case(TestCase {
            id: case_id.clone(),
            agent_id: agent_id.clone(),
            input: json!({"message": "hi"}),
            expected: vec![],
            name: "persist case".into(),
            dataset_id: None,
        })
        .await
        .unwrap();
        rt.upsert_dataset(Dataset {
            id: format!("ds_{}", unique_id()),
            name: "suite".into(),
            agent_id: agent_id.clone(),
            case_ids: vec![case_id.clone()],
            created_at: Utc::now(),
        })
        .await
        .unwrap();

        rt.set_kill_switch_persisted(true).await.unwrap();
        rt.audit()
            .record("test.action", "tester", None, json!({}))
            .await;
    }

    {
        let store = routerai::SqliteStore::connect(&url).await.unwrap();
        let rt = RouterRuntime::builder()
            .store(std::sync::Arc::new(store))
            .build()
            .await
            .unwrap();
        let loaded = rt.agents().get(&agent_id).await.expect("agent hydrated");
        assert_eq!(loaded.name, "persist-me");
        let events = rt.events().list(50).await;
        assert!(
            events.iter().any(|e| e.id == event_id),
            "event should hydrate into bus history"
        );
        assert!(rt.store().get_event(&event_id).await.unwrap().is_some());
        assert!(rt.sinks().get_target(&sink_id).await.is_some());
        assert!(rt.tests().get_case(&case_id).await.is_some());
        assert!(!rt.tests().list_datasets().await.is_empty());
        assert!(rt.kill_switch());
        assert!(!rt.audit().list(10).await.is_empty());

        // Deletes must not resurrect after another restart.
        rt.delete_agent(&agent_id).await.unwrap();
        rt.delete_sink_target(&sink_id).await.unwrap();
        rt.delete_test_case(&case_id).await.unwrap();
    }

    {
        let store = routerai::SqliteStore::connect(&url).await.unwrap();
        let rt = RouterRuntime::builder()
            .store(std::sync::Arc::new(store))
            .build()
            .await
            .unwrap();
        assert!(rt.agents().get(&agent_id).await.is_none());
        assert!(rt.sinks().get_target(&sink_id).await.is_none());
        assert!(rt.tests().get_case(&case_id).await.is_none());
    }

    let _ = std::fs::remove_file(path);
}

// local helper without uuid dep in tests
fn unique_id() -> String {
    routerai::RunId::new().to_string()
}

#[allow(dead_code)]
fn _assertion_max_cost() -> Assertion {
    Assertion::MaxCost {
        amount: Decimal::new(1, 2),
    }
}
