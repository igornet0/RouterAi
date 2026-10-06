//! Webhook EventSource → Handler → Agent → Webhook EventSink.

use std::sync::Arc;

use routerai::{Handler, RouterRuntime, RunStatus, SinkTarget};
use routerai_adapters::webhook::{
    ingress_event, HttpWebhookSink, WebhookIngressOptions, WebhookSinkConfig,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn webhook_roundtrip_source_to_sink() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/callback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1..)
        .mount(&mock)
        .await;

    let rt = RouterRuntime::builder().build().await.unwrap();
    let sink = Arc::new(
        HttpWebhookSink::new(rt.sinks().clone(), WebhookSinkConfig::default()).unwrap(),
    );
    rt.sinks().add_sink(sink).await;

    // Subscribe to agent.completed deliveries
    let mut target = SinkTarget::new(format!("{}/callback", mock.uri()), vec![
        "agent.completed".into(),
    ]);
    target.id = "sink_test".into();
    rt.sinks().upsert_target(target).await;

    let agent = routerai::published_agent("sales", "You are a sales agent.");
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await.unwrap();
    rt.upsert_handler(Handler::agent_on_event(
        "Webhook sales",
        "webhook.message.received",
        agent_id,
    ))
    .await
    .unwrap();

    let event = ingress_event(
        "message.received",
        json!({
            "customer": "123",
            "message": "Хочу узнать цену"
        }),
        WebhookIngressOptions {
            reply_to: Some(format!("{}/callback", mock.uri())),
            ..Default::default()
        },
    );
    assert_eq!(event.event_type, "webhook.message.received");
    assert_eq!(event.source, "webhook");

    let runs = rt.emit(event).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);

    // Allow async HTTP delivery
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let events = rt.events().list(50).await;
    assert!(events
        .iter()
        .any(|e| e.event_type == "webhook.message.received"));
    assert!(events.iter().any(|e| e.event_type == "agent.completed"));
}
