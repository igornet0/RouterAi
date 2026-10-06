//! Secrets never leave the secret store / outbound auth header: not in errors,
//! Debug output, persisted rows, statistics, key metadata, telemetry or tracing.
//! The mock provider echoes the key back in its error bodies, as some do.

use std::io::Write;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde_json::json;
use universal_ai::{
    AccountId, AddKeyRequest, AiClient, AttemptReport, BudgetPolicy, ModelId, ModelPricing,
    OpenAICompatible, ProviderCredential, ProviderId, RequestId, TelemetrySink, Usage,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Deliberately pattern-free (no `sk-` prefix): only exact-value redaction can
/// catch it.
const SECRET: &str = "PLAINTEXT_SECRET_7f3a9c";
const MANAGED: &str = "MANAGED_SECRET_c0ffee42";

fn echo_key(req: &Request) -> String {
    req.headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_string()
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl TelemetrySink for Recorder {
    fn request_started(&self, id: &RequestId, p: &ProviderId, m: &ModelId) {
        self.0.lock().unwrap().push(format!("{id} {p} {m}"));
    }
    fn request_completed(
        &self,
        id: &RequestId,
        p: &ProviderId,
        m: &ModelId,
        u: Option<&Usage>,
        l: u64,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{id} {p} {m} {u:?} {l}"));
    }
    fn request_failed(&self, id: &RequestId, message: &str) {
        self.0.lock().unwrap().push(format!("{id} {message}"));
    }
    fn attempt_finished(&self, report: &AttemptReport) {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_string(report).unwrap());
    }
}

fn assert_clean(what: &str, text: &str) {
    for secret in [SECRET, MANAGED] {
        assert!(!text.contains(secret), "secret leaked into {what}: {text}");
    }
}

#[tokio::test]
async fn secrets_do_not_leak_anywhere() {
    let logs = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // 1st call: 401 echoing the key; then a stream open failure echoing it; then
    // a 500 echoing it; everything else succeeds.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|req: &Request| {
            let key = echo_key(req);
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let text = body["messages"][0]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            match text.as_str() {
                "auth" => ResponseTemplate::new(401)
                    .set_body_string(format!("Incorrect API key provided: {key}")),
                "boom" => ResponseTemplate::new(500)
                    .set_body_string(format!("internal error for key={key} Bearer {key}")),
                _ => ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{ "message": { "role": "assistant", "content": "fine" } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                })),
            }
        })
        .mount(&server)
        .await;

    let recorder = Arc::new(Recorder::default());
    let provider = OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new(SECRET.into()))
        .provider_id(ProviderId::new("static"))
        .build()
        .unwrap();
    assert_clean("provider Debug", &format!("{provider:?}"));
    let client = AiClient::builder()
        .provider(provider)
        .telemetry(recorder.clone())
        .budget(BudgetPolicy::daily_usd(Decimal::TEN))
        .build()
        .unwrap();
    for id in ["static", "openai-compatible"] {
        client.pricing().upsert(ModelPricing::per_million(
            ProviderId::new(id),
            "m",
            Decimal::ONE,
            Decimal::ONE,
        ));
    }

    // Managed key on a second provider.
    let info = client
        .keys()
        .add_key(AddKeyRequest {
            provider: ProviderId::openai_compatible(),
            account_id: AccountId::new("acct"),
            secret: SecretString::new(MANAGED.into()),
            name: Some("managed".into()),
            base_url: Some(format!("{}/v1", server.uri())),
        })
        .await
        .unwrap();
    client
        .sync_provider_from_key(&info, info.base_url.as_deref())
        .await
        .unwrap();

    let mut errors = Vec::new();
    for provider in ["static", "openai-compatible"] {
        for text in ["auth", "boom", "ok"] {
            let builder = || {
                client
                    .chat()
                    .provider(provider)
                    .model("m")
                    .message(text)
                    .max_tokens(5)
            };
            if let Err(e) = builder().send().await {
                errors.push(format!("{e} {e:?}"));
            }
            match builder().stream().await {
                Err(e) => errors.push(format!("{e} {e:?}")),
                Ok(mut s) => {
                    while let Some(item) = s.next().await {
                        if let Err(e) = item {
                            errors.push(format!("{e} {e:?}"));
                        }
                    }
                }
            }
        }
    }
    assert!(errors.len() >= 8, "auth / 500 errors observed: {errors:?}");
    for e in &errors {
        assert_clean("error", e);
    }
    assert!(
        errors.iter().any(|e| e.contains("[REDACTED]")),
        "the echoed key reached the error text and was redacted: {errors:?}"
    );

    let rows = client.list_ai_requests(usize::MAX).await.unwrap();
    assert!(!rows.is_empty());
    assert_clean("persisted rows", &serde_json::to_string(&rows).unwrap());
    assert_clean("rows Debug", &format!("{rows:?}"));
    assert_clean(
        "statistics",
        &serde_json::to_string(&client.stats().all().await).unwrap(),
    );
    let keys = client.keys().list_keys().await;
    assert_clean("key metadata", &serde_json::to_string(&keys).unwrap());
    assert_clean("key metadata Debug", &format!("{keys:?}"));
    assert_clean("telemetry", &recorder.0.lock().unwrap().join("\n"));
    let credential = ProviderCredential::new(SecretString::new(SECRET.into()));
    assert_clean("credential Debug", &format!("{credential:?}"));

    drop(_guard);
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("attempt settled"), "tracing captured");
    assert_clean("tracing", &logs);
    // Prompt content is not logged by default either.
    assert!(!logs.contains("\"auth\"") && !logs.contains("message: boom"));
}
