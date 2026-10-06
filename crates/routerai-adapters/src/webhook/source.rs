//! Webhook → platform Event (no vendor types in core).

use async_trait::async_trait;
use routerai::{Event, EventSource};
use serde_json::Value;

/// Marker EventSource for webhook ingress.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebhookSource;

#[async_trait]
impl EventSource for WebhookSource {
    fn id(&self) -> &str {
        "webhook"
    }
}

/// Options when building an ingress event.
#[derive(Debug, Clone, Default)]
pub struct WebhookIngressOptions {
    /// Optional reply callback URL (delivered on matching completion events).
    pub reply_to: Option<String>,
    /// Optional account / tenant hint.
    pub account: Option<String>,
    /// Optional correlation id from caller.
    pub correlation_id: Option<String>,
}

/// Normalize path / request type to a platform event type.
///
/// - `message.received` → `webhook.message.received`
/// - `webhook.message.received` → unchanged
/// - empty → `webhook.message.received`
pub fn normalize_event_type(raw: &str) -> String {
    let t = raw.trim().trim_start_matches('/');
    if t.is_empty() {
        return "webhook.message.received".into();
    }
    if t.starts_with("webhook.") {
        t.to_string()
    } else {
        format!("webhook.{t}")
    }
}

/// Build a platform [`Event`] from webhook HTTP body.
pub fn ingress_event(
    event_type: &str,
    payload: Value,
    opts: WebhookIngressOptions,
) -> Event {
    let mut event = Event::new(normalize_event_type(event_type), "webhook", payload);
    if let Some(account) = opts.account {
        event = event.with_account(account);
    }
    if let Some(cid) = opts.correlation_id {
        event.correlation_id = Some(cid);
    }
    if let Some(reply_to) = opts.reply_to {
        event.metadata.extra.insert("reply_to".into(), reply_to);
    }
    event
}

/// Optional shared-secret check for ingress (`X-RouterAi-Webhook-Secret` or Bearer).
pub fn authorize_ingress(
    expected: Option<&str>,
    header_secret: Option<&str>,
    bearer: Option<&str>,
) -> bool {
    let Some(expected) = expected.filter(|s| !s.is_empty()) else {
        return true;
    };
    if header_secret == Some(expected) {
        return true;
    }
    if bearer == Some(expected) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_types() {
        assert_eq!(normalize_event_type(""), "webhook.message.received");
        assert_eq!(
            normalize_event_type("message.received"),
            "webhook.message.received"
        );
        assert_eq!(
            normalize_event_type("webhook.message.received"),
            "webhook.message.received"
        );
    }

    #[test]
    fn ingress_sets_reply_to() {
        let e = ingress_event(
            "message.received",
            json!({"message": "hi"}),
            WebhookIngressOptions {
                reply_to: Some("https://example.com/cb".into()),
                ..Default::default()
            },
        );
        assert_eq!(e.source, "webhook");
        assert_eq!(e.event_type, "webhook.message.received");
        assert_eq!(
            e.metadata.extra.get("reply_to").map(String::as_str),
            Some("https://example.com/cb")
        );
    }

    #[test]
    fn auth_optional_when_unset() {
        assert!(authorize_ingress(None, None, None));
        assert!(!authorize_ingress(Some("sec"), None, None));
        assert!(authorize_ingress(Some("sec"), Some("sec"), None));
        assert!(authorize_ingress(Some("sec"), None, Some("sec")));
    }
}
