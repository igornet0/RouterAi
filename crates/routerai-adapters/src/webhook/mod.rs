//! Webhook EventSource + EventSink.
//!
//! ```text
//! HTTP POST  →  WebhookSource  →  EventBus  →  Handler  →  Agent
//!                                                       ↓
//!                                              WebhookSink  →  HTTP POST
//! ```

mod sink;
mod source;

pub use sink::{CreateSinkTargetRequest, HttpWebhookSink, WebhookSinkConfig};
pub use source::{
    authorize_ingress, ingress_event, normalize_event_type, WebhookIngressOptions, WebhookSource,
};
