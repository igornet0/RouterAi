//! Platform Event → HTTP POST (webhook EventSink).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use routerai::{Event, EventSink, RouterError, RouterResult, SinkRegistry, SinkTarget};
use serde::{Deserialize, Serialize};

use crate::http::outbound_envelope;

/// Config for the HTTP webhook sink.
#[derive(Debug, Clone)]
pub struct WebhookSinkConfig {
    /// Request timeout.
    pub timeout: Duration,
    /// User-Agent.
    pub user_agent: String,
}

impl Default for WebhookSinkConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            user_agent: "routerai-webhook-sink/0.1".into(),
        }
    }
}

/// Delivers events to registered [`SinkTarget`] URLs and per-event `reply_to`.
pub struct HttpWebhookSink {
    client: Client,
    registry: SinkRegistry,
    /// Ephemeral reply URLs keyed by correlation / causation id.
    reply_routes: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
}

impl HttpWebhookSink {
    /// Create bound to a sink registry.
    pub fn new(registry: SinkRegistry, config: WebhookSinkConfig) -> RouterResult<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .user_agent(config.user_agent)
            .build()
            .map_err(|e| RouterError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            registry,
            reply_routes: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        })
    }

    /// Remember reply_to from an ingress event (correlation_id or event id).
    pub async fn remember_reply_to(&self, event: &Event) {
        let Some(url) = event.metadata.extra.get("reply_to").cloned() else {
            return;
        };
        let mut g = self.reply_routes.write().await;
        g.insert(event.id.to_string(), url.clone());
        if let Some(cid) = &event.correlation_id {
            g.insert(cid.clone(), url);
        }
    }

    /// Resolve reply URL for a completion / outbound event.
    async fn reply_url_for(&self, event: &Event) -> Option<String> {
        let g = self.reply_routes.read().await;
        if let Some(cid) = &event.correlation_id {
            if let Some(u) = g.get(cid) {
                return Some(u.clone());
            }
        }
        if let Some(cause) = &event.causation_id {
            if let Some(u) = g.get(cause) {
                return Some(u.clone());
            }
        }
        event.metadata.extra.get("reply_to").cloned()
    }

    async fn post_json(
        &self,
        url: &str,
        secret: Option<&str>,
        body: &serde_json::Value,
    ) -> RouterResult<()> {
        let mut req = self.client.post(url).json(body);
        if let Some(secret) = secret {
            req = req.header("X-RouterAi-Sink-Secret", secret);
        }
        let res = req
            .send()
            .await
            .map_err(|e| RouterError::Internal(format!("webhook sink: {e}")))?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(RouterError::Internal(format!(
                "webhook sink HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl EventSink for HttpWebhookSink {
    fn id(&self) -> &str {
        "webhook.http"
    }

    async fn deliver(&self, event: &Event) -> RouterResult<()> {
        // Capture reply routes from ingress events.
        if event.source == "webhook" || event.metadata.extra.contains_key("reply_to") {
            self.remember_reply_to(event).await;
        }

        let body = outbound_envelope(event);
        let mut last_err: Option<RouterError> = None;
        let mut delivered = false;

        for target in self.registry.matching_targets(&event.event_type).await {
            match self
                .post_json(&target.url, target.secret.as_deref(), &body)
                .await
            {
                Ok(()) => {
                    delivered = true;
                    tracing::info!(
                        target_id = %target.id,
                        url = %target.url,
                        event_type = %event.event_type,
                        "webhook sink delivered"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        target_id = %target.id,
                        error = %err,
                        "webhook target failed"
                    );
                    last_err = Some(err);
                }
            }
        }

        if event.event_type.starts_with("agent.") {
            if let Some(url) = self.reply_url_for(event).await {
                match self.post_json(&url, None, &body).await {
                    Ok(()) => {
                        delivered = true;
                        tracing::info!(
                            %url,
                            event_type = %event.event_type,
                            "webhook reply_to delivered"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(%url, error = %err, "webhook reply_to failed");
                        last_err = Some(err);
                    }
                }
            }
        }

        if !delivered {
            if let Some(err) = last_err {
                return Err(err);
            }
            tracing::debug!(event_type = %event.event_type, "no webhook targets matched");
        }
        Ok(())
    }
}

/// Request body for creating a sink target via API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSinkTargetRequest {
    /// Callback URL.
    pub url: String,
    /// Event types (supports `agent.*` suffix wildcard).
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Optional secret.
    #[serde(default)]
    pub secret: Option<String>,
    /// Enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl CreateSinkTargetRequest {
    /// Into [`SinkTarget`].
    pub fn into_target(self) -> SinkTarget {
        let mut t = SinkTarget::new(self.url, self.event_types);
        t.secret = self.secret;
        t.enabled = self.enabled;
        t
    }
}
