//! Per-request API key binding: the key selected for a request is the key sent to
//! the provider and the key recorded in statistics.
//!
//! Mock servers echo the credential they received, so each assertion compares what
//! actually went over the wire with what `AiClient` recorded. Secrets are fake.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use serde_json::json;
use universal_ai::{
    AccountId, AddKeyRequest, AiClient, AiConfig, AiError, KeyId, KeySelectionStrategy,
    ProviderCredential, ProviderId, RetryPolicy,
};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY_A: &str = "KEY_A_TEST";
const KEY_B: &str = "KEY_B_TEST";
const KEY_C: &str = "KEY_C_TEST";

/// Which wire format / auth header a mock speaks.
#[derive(Clone, Copy)]
enum Wire {
    OpenAiCompatible,
    Anthropic,
    Gemini,
}

impl Wire {
    fn provider(self) -> ProviderId {
        match self {
            Wire::OpenAiCompatible => ProviderId::openai_compatible(),
            Wire::Anthropic => ProviderId::anthropic(),
            Wire::Gemini => ProviderId::gemini(),
        }
    }

    fn base_url(self, server: &MockServer) -> String {
        match self {
            Wire::OpenAiCompatible => format!("{}/v1", server.uri()),
            Wire::Anthropic | Wire::Gemini => server.uri(),
        }
    }

    /// Credential as received by the provider endpoint.
    fn credential(self, req: &Request) -> String {
        let header = |name: &str| {
            req.headers
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        match self {
            Wire::OpenAiCompatible => header("authorization")
                .trim_start_matches("Bearer ")
                .to_string(),
            Wire::Anthropic => header("x-api-key"),
            Wire::Gemini => header("x-goog-api-key"),
        }
    }

    /// Successful reply whose text is the credential the server saw.
    fn echo(self, text: &str) -> ResponseTemplate {
        let body = match self {
            Wire::OpenAiCompatible => json!({
                "choices": [{ "message": { "role": "assistant", "content": text },
                              "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 }
            }),
            Wire::Anthropic => json!({
                "content": [{ "type": "text", "text": text }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 3, "output_tokens": 1 }
            }),
            Wire::Gemini => json!({
                "candidates": [{ "content": { "role": "model", "parts": [{ "text": text }] },
                                 "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 1 }
            }),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }

    async fn mount(self, server: &MockServer) {
        let route = match self {
            Wire::OpenAiCompatible => Mock::given(method("POST")).and(path("/v1/chat/completions")),
            Wire::Anthropic => Mock::given(method("POST")).and(path("/v1/messages")),
            Wire::Gemini => {
                Mock::given(method("POST")).and(path_regex(r"^/models/[^/]+:generateContent$"))
            }
        };
        route
            .respond_with(move |req: &Request| self.echo(&self.credential(req)))
            .mount(server)
            .await;
    }
}

struct Setup {
    server: MockServer,
    client: AiClient,
    wire: Wire,
    /// secret → key id
    ids: HashMap<String, KeyId>,
}

impl Setup {
    async fn new(wire: Wire, strategy: KeySelectionStrategy, secrets: &[&str]) -> Self {
        let server = MockServer::start().await;
        wire.mount(&server).await;
        Self::with_server(server, wire, strategy, secrets, AiConfig::default()).await
    }

    async fn with_server(
        server: MockServer,
        wire: Wire,
        strategy: KeySelectionStrategy,
        secrets: &[&str],
        config: AiConfig,
    ) -> Self {
        let client = AiClient::builder()
            .config(config)
            .allow_empty_providers()
            .build()
            .unwrap();
        client.keys().set_selection_strategy(strategy).await;
        let mut setup = Self {
            server,
            client,
            wire,
            ids: HashMap::new(),
        };
        for secret in secrets {
            setup.add_key(secret).await;
        }
        setup
    }

    /// Add a managed key the way the server API does (add + sync provider).
    async fn add_key(&mut self, secret: &str) -> KeyId {
        let info = self
            .client
            .keys()
            .add_key(AddKeyRequest {
                provider: self.wire.provider(),
                account_id: AccountId::new(format!("acct-{}", self.ids.len() + 1)),
                secret: SecretString::new(secret.into()),
                name: Some(format!("key-{}", self.ids.len() + 1)),
                base_url: Some(self.wire.base_url(&self.server)),
            })
            .await
            .unwrap();
        self.client
            .sync_provider_from_key(&info, info.base_url.as_deref())
            .await
            .unwrap();
        self.ids.insert(secret.to_string(), info.id.clone());
        info.id
    }

    async fn send(&self) -> Result<universal_ai::ChatResponse, AiError> {
        self.client
            .chat()
            .model("test-model")
            .message("hi")
            .send()
            .await
    }

    /// One request; asserts wire credential == response echo == recorded key id.
    async fn request(&self) -> String {
        let response = self.send().await.unwrap();
        let echoed = response.text();
        let row = self
            .client
            .get_ai_request(&response.request_id)
            .await
            .unwrap()
            .expect("request persisted");
        assert_eq!(
            row.api_key.as_ref(),
            self.ids.get(&echoed),
            "statistics must name the key that was actually sent"
        );
        echoed
    }

    async fn wire_credentials(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| self.wire.credential(r))
            .collect()
    }
}

// ---------- A. round-robin ----------

#[tokio::test]
async fn a_round_robin_rotates_in_order() {
    let s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::RoundRobin,
        &[KEY_A, KEY_B, KEY_C],
    )
    .await;

    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(s.request().await);
    }
    let expected = [KEY_A, KEY_B, KEY_C, KEY_A, KEY_B, KEY_C];
    assert_eq!(seen, expected);
    assert_eq!(s.wire_credentials().await, expected);
}

#[tokio::test]
async fn round_robin_skips_disabled_keys() {
    let s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::RoundRobin,
        &[KEY_A, KEY_B, KEY_C],
    )
    .await;
    s.client.keys().disable_key(&s.ids[KEY_B]).await.unwrap();

    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(s.request().await);
    }
    assert_eq!(seen, [KEY_A, KEY_C, KEY_A, KEY_C]);
}

// ---------- B. single key ----------

#[tokio::test]
async fn b_single_key_used_for_every_request() {
    let s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::RoundRobin,
        &[KEY_A],
    )
    .await;
    for _ in 0..3 {
        assert_eq!(s.request().await, KEY_A);
    }
    assert_eq!(s.wire_credentials().await, [KEY_A, KEY_A, KEY_A]);
}

// ---------- C. delete the active key ----------

#[tokio::test]
async fn c_deleted_key_is_never_used_again() {
    let s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::FirstAvailable,
        &[KEY_A, KEY_B],
    )
    .await;

    assert_eq!(s.request().await, KEY_A);
    s.client.keys().remove_key(&s.ids[KEY_A]).await.unwrap();
    assert_eq!(s.request().await, KEY_B);
    assert_eq!(s.request().await, KEY_B);

    // Last key gone: the provider has no credential left — explicit failure,
    // nothing is sent with a stale key.
    s.client.keys().remove_key(&s.ids[KEY_B]).await.unwrap();
    let err = s.send().await.unwrap_err();
    assert!(matches!(err, AiError::Authentication { .. }), "{err:?}");
    assert_eq!(s.wire_credentials().await, [KEY_A, KEY_B, KEY_B]);
}

// ---------- D. provider isolation ----------

#[tokio::test]
async fn d_selected_key_wins_over_last_registered() {
    // FirstAvailable must keep using A even though B was synced last
    // (previously the provider was rebuilt with B's secret).
    let mut s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::FirstAvailable,
        &[KEY_A],
    )
    .await;
    assert_eq!(s.request().await, KEY_A);
    s.add_key(KEY_B).await;
    assert_eq!(s.request().await, KEY_A);
    assert_eq!(s.request().await, KEY_A);

    // Same client, switch strategy: selection — not registration order — decides.
    s.client
        .keys()
        .set_selection_strategy(KeySelectionStrategy::RoundRobin)
        .await;
    let pair = [s.request().await, s.request().await];
    assert!(pair.contains(&KEY_A.to_string()) && pair.contains(&KEY_B.to_string()));
}

#[tokio::test]
async fn static_provider_key_still_works_without_managed_keys() {
    let server = MockServer::start().await;
    Wire::OpenAiCompatible.mount(&server).await;
    let provider = universal_ai::OpenAICompatible::builder()
        .base_url(format!("{}/v1", server.uri()))
        .api_key(SecretString::new(KEY_C.into()))
        .build()
        .unwrap();
    let client = AiClient::builder().provider(provider).build().unwrap();
    let response = client.chat().model("m").message("hi").send().await.unwrap();
    assert_eq!(response.text(), KEY_C);
    let row = client
        .get_ai_request(&response.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.api_key, None, "static credential is not a managed key");
}

// ---------- E. statistics ----------

#[tokio::test]
async fn e_statistics_follow_the_key_actually_used() {
    let s = Setup::new(
        Wire::OpenAiCompatible,
        KeySelectionStrategy::RoundRobin,
        &[KEY_A, KEY_B, KEY_C],
    )
    .await;
    for _ in 0..6 {
        s.request().await; // asserts row.api_key per request
    }
    for (secret, id) in &s.ids {
        let info = s.client.keys().get_key(id).await.unwrap();
        assert_eq!(info.usage.requests, 2, "{secret}");
        assert_eq!(info.usage.total_tokens, 8, "{secret}");
        assert!(info.last_used_at.is_some());
    }
    let rows = s.client.list_ai_requests(10).await.unwrap();
    assert_eq!(rows.len(), 6);
    for row in rows {
        let account = s
            .client
            .keys()
            .get_key(row.api_key.as_ref().unwrap())
            .await
            .unwrap()
            .account_id;
        assert_eq!(row.account, account, "account follows the selected key");
    }
}

#[tokio::test]
async fn retries_reuse_the_selected_key() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503).set_body_string("overloaded")
            } else {
                Wire::OpenAiCompatible.echo(&Wire::OpenAiCompatible.credential(req))
            }
        })
        .mount(&server)
        .await;
    let config = AiConfig {
        retry_policy: RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            exponential_backoff: false,
        },
        ..AiConfig::default()
    };
    let s = Setup::with_server(
        server,
        Wire::OpenAiCompatible,
        KeySelectionStrategy::RoundRobin,
        &[KEY_A, KEY_B],
        config,
    )
    .await;

    // Both physical attempts of the first logical request carry A.
    assert_eq!(s.request().await, KEY_A);
    assert_eq!(s.wire_credentials().await, [KEY_A, KEY_A]);
    // Rotation advances per logical request, not per retry.
    assert_eq!(s.request().await, KEY_B);
}

// ---------- F. concurrency ----------

#[tokio::test]
async fn f_concurrent_requests_keep_their_own_credentials() {
    let server = MockServer::start().await;
    // Slow replies keep many requests in flight at once.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            Wire::OpenAiCompatible
                .echo(&Wire::OpenAiCompatible.credential(req))
                .set_delay(Duration::from_millis(50))
        })
        .mount(&server)
        .await;
    let s = Arc::new(
        Setup::with_server(
            server,
            Wire::OpenAiCompatible,
            KeySelectionStrategy::RoundRobin,
            &[KEY_A, KEY_B, KEY_C],
            AiConfig::default(),
        )
        .await,
    );

    let handles: Vec<_> = (0..12)
        .map(|_| {
            let s = Arc::clone(&s);
            tokio::spawn(async move { s.request().await })
        })
        .collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for h in handles {
        *counts.entry(h.await.unwrap()).or_default() += 1;
    }
    assert_eq!(counts.len(), 3);
    assert!(counts.values().all(|n| *n == 4), "{counts:?}");
}

#[tokio::test]
async fn deleting_a_key_during_traffic_never_reuses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            Wire::OpenAiCompatible
                .echo(&Wire::OpenAiCompatible.credential(req))
                .set_delay(Duration::from_millis(20))
        })
        .mount(&server)
        .await;
    let s = Arc::new(
        Setup::with_server(
            server,
            Wire::OpenAiCompatible,
            KeySelectionStrategy::RoundRobin,
            &[KEY_A, KEY_B],
            AiConfig::default(),
        )
        .await,
    );

    let in_flight: Vec<_> = (0..6)
        .map(|_| {
            let s = Arc::clone(&s);
            tokio::spawn(async move { s.request().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(5)).await;
    s.client.keys().remove_key(&s.ids[KEY_A]).await.unwrap();
    // Requests already bound to A may finish with it (per-request snapshot); each
    // still records the key it really used (asserted inside `request`).
    for h in in_flight {
        h.await.unwrap();
    }

    // Every request started after deletion uses B.
    for _ in 0..4 {
        assert_eq!(s.request().await, KEY_B);
    }
    let wire = s.wire_credentials().await;
    assert_eq!(&wire[wire.len() - 4..], [KEY_B; 4]);
}

// ---------- G. no secret leakage ----------

#[tokio::test]
async fn g_secret_not_leaked_in_errors_or_debug() {
    let server = MockServer::start().await;
    // A provider that echoes the bad key back in its error body.
    Mock::given(method("POST"))
        .respond_with(|req: &Request| {
            let key = Wire::OpenAiCompatible.credential(req);
            ResponseTemplate::new(401)
                .set_body_string(format!("Incorrect API key provided: {key}. Check {key}."))
        })
        .mount(&server)
        .await;
    let s = Setup::with_server(
        server,
        Wire::OpenAiCompatible,
        KeySelectionStrategy::FirstAvailable,
        &[KEY_A],
        AiConfig::default(),
    )
    .await;

    let err = s.send().await.unwrap_err();
    assert!(matches!(err, AiError::Authentication { .. }));
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains(KEY_A), "secret leaked: {text}");
    }
    assert!(format!("{err:?}").contains("[REDACTED]"));

    let rows = s.client.list_ai_requests(5).await.unwrap();
    let persisted = serde_json::to_string(&rows).unwrap();
    assert!(!persisted.contains(KEY_A));
    let keys = format!("{:?}", s.client.keys().list_keys().await);
    assert!(!keys.contains(KEY_A));

    let cred = ProviderCredential::for_key(
        s.ids[KEY_A].clone(),
        SecretString::new(KEY_A.into()),
        Some("https://example.test".into()),
    );
    assert!(!format!("{cred:?}").contains(KEY_A));
}

#[tokio::test]
async fn gemini_key_never_appears_in_url() {
    let s = Setup::new(Wire::Gemini, KeySelectionStrategy::FirstAvailable, &[KEY_A]).await;
    s.request().await;
    let reqs = s.server.received_requests().await.unwrap();
    assert!(
        reqs.iter().all(|r| r.url.query().is_none()),
        "key must be a header"
    );
}

// ---------- H. provider coverage ----------

async fn rotation_for(wire: Wire) {
    let s = Setup::new(wire, KeySelectionStrategy::RoundRobin, &[KEY_A, KEY_B]).await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(s.request().await);
    }
    assert_eq!(seen, [KEY_A, KEY_B, KEY_A, KEY_B]);
    assert_eq!(s.wire_credentials().await, [KEY_A, KEY_B, KEY_A, KEY_B]);

    s.client.keys().remove_key(&s.ids[KEY_A]).await.unwrap();
    assert_eq!(s.request().await, KEY_B);
    assert_eq!(s.request().await, KEY_B);
}

#[tokio::test]
async fn h_openai_compatible_binds_authorization_header() {
    rotation_for(Wire::OpenAiCompatible).await;
}

#[tokio::test]
async fn h_anthropic_binds_x_api_key() {
    rotation_for(Wire::Anthropic).await;
}

#[tokio::test]
async fn h_gemini_binds_x_goog_api_key() {
    rotation_for(Wire::Gemini).await;
}

#[tokio::test]
async fn key_base_url_is_where_its_secret_goes() {
    // Two keys for the same provider id on different endpoints: each secret must
    // only reach its own endpoint.
    let east = MockServer::start().await;
    let west = MockServer::start().await;
    Wire::OpenAiCompatible.mount(&east).await;
    Wire::OpenAiCompatible.mount(&west).await;

    let client = AiClient::builder().allow_empty_providers().build().unwrap();
    client
        .keys()
        .set_selection_strategy(KeySelectionStrategy::RoundRobin)
        .await;
    for (secret, server) in [(KEY_A, &east), (KEY_B, &west)] {
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
    }
    for _ in 0..4 {
        client.chat().model("m").message("hi").send().await.unwrap();
    }
    let creds = |reqs: Vec<Request>| -> Vec<String> {
        reqs.iter()
            .map(|r| Wire::OpenAiCompatible.credential(r))
            .collect()
    };
    assert_eq!(
        creds(east.received_requests().await.unwrap()),
        [KEY_A, KEY_A]
    );
    assert_eq!(
        creds(west.received_requests().await.unwrap()),
        [KEY_B, KEY_B]
    );
}
