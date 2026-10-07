//! Declarative handlers: Event → conditions → actions[].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::error::{RouterError, RouterResult};
use crate::event::Event;
use crate::ids::{AgentId, HandlerId};

/// When a handler fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerTrigger {
    /// Exact event type match.
    pub event: String,
}

/// Comparison operator for conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    /// Field exists (non-null).
    Exists,
    /// Equals JSON value.
    Eq,
    /// String contains.
    Contains,
    /// Numeric / string not equals.
    Neq,
}

/// Condition on event JSON path (`payload.text`, `metadata.account`, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// Dotted path from event root.
    pub field: String,
    /// Operator.
    pub operator: ConditionOp,
    /// Optional comparison value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Action executed when handler matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Start an agent run.
    AgentRun {
        /// Agent id.
        agent_id: AgentId,
    },
    /// Emit a follow-up event.
    EventEmit {
        /// New event type.
        event_type: String,
        /// Optional payload override (defaults to parent payload).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        /// Source label.
        #[serde(default = "default_source")]
        source: String,
    },
    /// Invoke a tool directly.
    ToolCall {
        /// Tool id.
        tool_id: String,
        /// Input JSON.
        #[serde(default)]
        input: Value,
    },
}

fn default_source() -> String {
    "handler".into()
}

/// What to do on action failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// Stop remaining actions.
    #[default]
    FailFast,
    /// Continue other actions.
    Continue,
    /// Emit `handler.failed` and stop.
    EmitFailedEvent,
}

/// Declarative automation unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handler {
    /// Id.
    pub id: HandlerId,
    /// Display name.
    pub name: String,
    /// Enabled flag.
    pub enabled: bool,
    /// Trigger.
    pub trigger: HandlerTrigger,
    /// Conditions (AND).
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Actions in order.
    pub actions: Vec<Action>,
    /// Max attempts for transient failures (engine-level).
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Timeout seconds for the whole handler invocation.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Max concurrent invocations (0 = unlimited).
    #[serde(default)]
    pub concurrency: u32,
    /// Error policy.
    #[serde(default)]
    pub error_policy: ErrorPolicy,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Updated.
    pub updated_at: DateTime<Utc>,
}

fn default_retries() -> u32 {
    1
}
fn default_timeout() -> u64 {
    60
}

impl Handler {
    /// Simple agent-run handler.
    pub fn agent_on_event(
        name: impl Into<String>,
        event_type: impl Into<String>,
        agent_id: impl Into<AgentId>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: HandlerId::new(),
            name: name.into(),
            enabled: true,
            trigger: HandlerTrigger {
                event: event_type.into(),
            },
            conditions: vec![Condition {
                field: "payload".into(),
                operator: ConditionOp::Exists,
                value: None,
            }],
            actions: vec![Action::AgentRun {
                agent_id: agent_id.into(),
            }],
            max_retries: 1,
            timeout_secs: 60,
            concurrency: 0,
            error_policy: ErrorPolicy::FailFast,
            created_at: now,
            updated_at: now,
        }
    }

    /// Evaluate conditions against an event.
    pub fn matches(&self, event: &Event) -> bool {
        if !self.enabled {
            return false;
        }
        if self.trigger.event != event.event_type {
            return false;
        }
        self.conditions.iter().all(|c| eval_condition(c, event))
    }
}

fn eval_condition(cond: &Condition, event: &Event) -> bool {
    let Ok(root) = serde_json::to_value(event) else {
        return false;
    };
    let actual = json_path(&root, &cond.field);
    match cond.operator {
        ConditionOp::Exists => !actual.is_null(),
        ConditionOp::Eq => cond.value.as_ref().is_some_and(|v| &actual == v),
        ConditionOp::Neq => cond.value.as_ref().is_some_and(|v| &actual != v),
        ConditionOp::Contains => match (
            actual.as_str(),
            cond.value.as_ref().and_then(|v| v.as_str()),
        ) {
            (Some(hay), Some(needle)) => hay.contains(needle),
            _ => false,
        },
    }
}

fn json_path(value: &Value, path: &str) -> Value {
    let mut cur = value;
    for part in path.split('.') {
        match cur {
            Value::Object(map) => {
                cur = map.get(part).unwrap_or(&Value::Null);
            }
            _ => return Value::Null,
        }
    }
    cur.clone()
}

/// Planned action after matching.
#[derive(Debug, Clone)]
pub struct MatchedHandler {
    /// Handler.
    pub handler: Handler,
    /// Triggering event.
    pub event: Event,
}

/// In-memory handler registry + matcher.
#[derive(Clone, Default)]
pub struct HandlerEngine {
    handlers: Arc<RwLock<HashMap<String, Handler>>>,
}

impl HandlerEngine {
    /// Empty engine.
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert handler.
    pub async fn upsert(&self, handler: Handler) -> RouterResult<()> {
        if handler.actions.is_empty() {
            return Err(RouterError::Invalid(
                "handler must have at least one action".into(),
            ));
        }
        self.handlers
            .write()
            .await
            .insert(handler.id.to_string(), handler);
        Ok(())
    }

    /// Remove.
    pub async fn remove(&self, id: &HandlerId) -> RouterResult<()> {
        if self.handlers.write().await.remove(id.as_str()).is_none() {
            return Err(RouterError::NotFound(format!("handler {id}")));
        }
        Ok(())
    }

    /// List.
    pub async fn list(&self) -> Vec<Handler> {
        self.handlers.read().await.values().cloned().collect()
    }

    /// Get.
    pub async fn get(&self, id: &HandlerId) -> Option<Handler> {
        self.handlers.read().await.get(id.as_str()).cloned()
    }

    /// Find all matching handlers for an event.
    pub async fn match_event(&self, event: &Event) -> Vec<MatchedHandler> {
        self.handlers
            .read()
            .await
            .values()
            .filter(|h| h.matches(event))
            .map(|h| MatchedHandler {
                handler: h.clone(),
                event: event.clone(),
            })
            .collect()
    }

    /// Timeout helper.
    pub fn timeout_of(handler: &Handler) -> Duration {
        Duration::from_secs(handler.timeout_secs.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn condition_contains_text() {
        let h = Handler {
            conditions: vec![Condition {
                field: "payload.text".into(),
                operator: ConditionOp::Contains,
                value: Some(json!("купить")),
            }],
            ..Handler::agent_on_event("s", "telegram.message.received", "sales")
        };
        let mut ok = Event::new(
            "telegram.message.received",
            "telegram",
            json!({"text": "Хочу купить продукт"}),
        );
        assert!(h.matches(&ok));
        ok.payload = json!({"text": "привет"});
        assert!(!h.matches(&ok));
    }
}
