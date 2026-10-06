//! Tool registry + executor (LLM does not execute tools directly).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{RouterError, RouterResult};
use crate::ids::ToolId;

/// Permission flags for tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Network egress.
    Network,
    /// Read filesystem.
    FsRead,
    /// Write filesystem.
    FsWrite,
    /// Shell execution.
    Shell,
    /// Emit events.
    EventEmit,
    /// Call models.
    Ai,
    /// Messaging send.
    Messaging,
}

/// Set of permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Allowed.
    #[serde(default)]
    pub allow: HashSet<Permission>,
}

impl Permissions {
    /// Empty (deny all sensitive).
    pub fn none() -> Self {
        Self::default()
    }

    /// AI + event emit.
    pub fn ai_safe() -> Self {
        let mut allow = HashSet::new();
        allow.insert(Permission::Ai);
        allow.insert(Permission::EventEmit);
        Self { allow }
    }

    /// Network read tools.
    pub fn network() -> Self {
        let mut allow = HashSet::new();
        allow.insert(Permission::Network);
        Self { allow }
    }

    /// Check.
    pub fn allows(&self, p: Permission) -> bool {
        self.allow.contains(&p)
    }
}

/// Tool metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Id (`web.search`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description for agents.
    pub description: String,
    /// JSON Schema input.
    pub input_schema: Value,
    /// Optional output schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Required permissions.
    #[serde(default)]
    pub permissions: Permissions,
}

/// Runtime tool handler.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Execute.
    async fn call(&self, input: Value) -> RouterResult<Value>;
}

/// Registry of definitions + handlers.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    defs: Arc<RwLock<HashMap<String, ToolDefinition>>>,
    handlers: Arc<RwLock<HashMap<String, Arc<dyn ToolHandler>>>>,
}

impl ToolRegistry {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register.
    pub async fn register(
        &self,
        def: ToolDefinition,
        handler: Arc<dyn ToolHandler>,
    ) -> RouterResult<()> {
        let id = def.id.clone();
        self.defs.write().await.insert(id.clone(), def);
        self.handlers.write().await.insert(id, handler);
        Ok(())
    }

    /// List definitions.
    pub async fn list(&self) -> Vec<ToolDefinition> {
        self.defs.read().await.values().cloned().collect()
    }

    /// Get definition.
    pub async fn get(&self, id: &str) -> Option<ToolDefinition> {
        self.defs.read().await.get(id).cloned()
    }
}

/// Executes tools with agent permission intersection.
#[derive(Clone)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    /// Global deny list (kill / policy).
    global_deny: Arc<RwLock<HashSet<Permission>>>,
}

impl ToolExecutor {
    /// Create.
    pub fn new(registry: ToolRegistry) -> Self {
        // Deny shell + fs write by default at runtime level.
        let mut deny = HashSet::new();
        deny.insert(Permission::Shell);
        deny.insert(Permission::FsWrite);
        Self {
            registry,
            global_deny: Arc::new(RwLock::new(deny)),
        }
    }

    /// Borrow registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Kill switch: deny a permission globally.
    pub async fn deny(&self, p: Permission) {
        self.global_deny.write().await.insert(p);
    }

    /// Execute if allowed.
    pub async fn execute(
        &self,
        tool_id: &str,
        input: Value,
        agent_allowed_tools: &[String],
    ) -> RouterResult<Value> {
        if !agent_allowed_tools.is_empty() && !agent_allowed_tools.iter().any(|t| t == tool_id) {
            return Err(RouterError::PermissionDenied(format!(
                "tool {tool_id} not in agent allow-list"
            )));
        }
        let def = self
            .registry
            .get(tool_id)
            .await
            .ok_or_else(|| RouterError::NotFound(format!("tool {tool_id}")))?;

        let deny = self.global_deny.read().await;
        for p in &def.permissions.allow {
            if deny.contains(p) {
                return Err(RouterError::PermissionDenied(format!(
                    "permission {p:?} denied by runtime policy"
                )));
            }
        }
        drop(deny);

        let handler = self
            .registry
            .handlers
            .read()
            .await
            .get(tool_id)
            .cloned()
            .ok_or_else(|| RouterError::NotFound(format!("tool handler {tool_id}")))?;

        tracing::info!(tool_id, "executing tool");
        handler.call(input).await
    }
}

/// Builtin core tools.
pub struct BuiltinTools;

impl BuiltinTools {
    /// Register safe builtins into a registry.
    pub async fn register_all(registry: &ToolRegistry) -> RouterResult<()> {
        registry
            .register(
                ToolDefinition {
                    id: "event.emit".into(),
                    name: "Emit Event".into(),
                    description: "Emit a follow-up platform event".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "event_type": { "type": "string" },
                            "payload": {}
                        },
                        "required": ["event_type"]
                    }),
                    output_schema: None,
                    permissions: Permissions::ai_safe(),
                },
                Arc::new(EventEmitTool),
            )
            .await?;

        registry
            .register(
                ToolDefinition {
                    id: "json.echo".into(),
                    name: "JSON Echo".into(),
                    description: "Return input unchanged (test helper)".into(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    permissions: Permissions::none(),
                },
                Arc::new(EchoTool),
            )
            .await?;

        registry
            .register(
                ToolDefinition {
                    id: "web.search".into(),
                    name: "Web Search".into(),
                    description: "Stub web search (returns placeholder results)".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": { "query": { "type": "string" } },
                        "required": ["query"]
                    }),
                    output_schema: None,
                    permissions: Permissions::network(),
                },
                Arc::new(WebSearchStub),
            )
            .await?;

        let _ = ToolId::new(); // keep id type used
        Ok(())
    }
}

struct EchoTool;
#[async_trait]
impl ToolHandler for EchoTool {
    async fn call(&self, input: Value) -> RouterResult<Value> {
        Ok(input)
    }
}

struct EventEmitTool;
#[async_trait]
impl ToolHandler for EventEmitTool {
    async fn call(&self, input: Value) -> RouterResult<Value> {
        // Actual emit is done by runtime; this returns a plan.
        Ok(json!({ "planned": input, "status": "planned" }))
    }
}

struct WebSearchStub;
#[async_trait]
impl ToolHandler for WebSearchStub {
    async fn call(&self, input: Value) -> RouterResult<Value> {
        let q = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(json!({
            "query": q,
            "results": [
                { "title": "Stub result", "url": "https://example.com", "snippet": "Placeholder" }
            ]
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn denies_unlisted_tool() {
        let reg = ToolRegistry::new();
        BuiltinTools::register_all(&reg).await.unwrap();
        let exec = ToolExecutor::new(reg);
        let err = exec
            .execute("json.echo", json!({"a": 1}), &["web.search".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, RouterError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn echo_works() {
        let reg = ToolRegistry::new();
        BuiltinTools::register_all(&reg).await.unwrap();
        let exec = ToolExecutor::new(reg);
        let out = exec
            .execute("json.echo", json!({"a": 1}), &["json.echo".into()])
            .await
            .unwrap();
        assert_eq!(out["a"], 1);
    }
}
