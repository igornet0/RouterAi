//! Tools offered to the model during an agent run.
//!
//! Tool ids (`web.search`) are not valid function names for every provider
//! (OpenAI / Anthropic allow `[A-Za-z0-9_-]{1,64}`), so each run maps a safe
//! function name back to the registered tool id. Only the agent policy decides
//! what is offered; run input never adds tools.

use std::collections::HashMap;

use universal_ai::Tool;

use crate::agent::Agent;
use crate::tool::ToolExecutor;

const MAX_FUNCTION_NAME: usize = 64;

/// Tools an agent may call in one run.
#[derive(Debug, Default)]
pub(crate) struct AgentToolset {
    tools: Vec<Tool>,
    by_function: HashMap<String, String>,
}

impl AgentToolset {
    /// Build from the agent allow-list. Returns the toolset plus tools withheld
    /// by policy (`(tool_id, reason)`), so the trace shows why they are missing.
    pub(crate) async fn resolve(
        executor: &ToolExecutor,
        agent: &Agent,
    ) -> (Self, Vec<(String, String)>) {
        let mut set = Self::default();
        let mut withheld = Vec::new();
        for tool_id in &agent.tools {
            if set.by_function.values().any(|id| id == tool_id) {
                continue;
            }
            match executor.authorize_for_agent(tool_id, agent).await {
                Ok(def) => {
                    let name = set.unique_name(tool_id);
                    set.tools.push(def.to_model_tool(name.clone()));
                    set.by_function.insert(name, tool_id.clone());
                }
                Err(err) => withheld.push((tool_id.clone(), err.to_string())),
            }
        }
        (set, withheld)
    }

    /// Tool schemas for the model request.
    pub(crate) fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Registered tool id for a model-provided function name.
    pub(crate) fn tool_id(&self, function_name: &str) -> Option<&str> {
        self.by_function.get(function_name).map(String::as_str)
    }

    fn unique_name(&self, tool_id: &str) -> String {
        let base = function_name(tool_id);
        if !self.by_function.contains_key(&base) {
            return base;
        }
        (2..)
            .map(|n| {
                let suffix = format!("_{n}");
                let keep = MAX_FUNCTION_NAME.saturating_sub(suffix.len());
                format!("{}{suffix}", &base[..base.len().min(keep)])
            })
            .find(|candidate| !self.by_function.contains_key(candidate))
            .unwrap_or(base)
    }
}

/// Provider-safe function name for a tool id (`web.search` → `web_search`).
pub(crate) fn function_name(tool_id: &str) -> String {
    let mut name: String = tool_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_FUNCTION_NAME)
        .collect();
    if name.is_empty() {
        name.push_str("tool");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_names_are_provider_safe() {
        assert_eq!(function_name("web.search"), "web_search");
        assert_eq!(function_name("crm/lookup v2"), "crm_lookup_v2");
        assert_eq!(function_name(""), "tool");
        assert_eq!(function_name(&"x".repeat(100)).len(), MAX_FUNCTION_NAME);
    }

    #[test]
    fn colliding_names_get_suffix() {
        let mut set = AgentToolset::default();
        set.by_function.insert("a_b".into(), "a.b".into());
        assert_eq!(set.unique_name("a/b"), "a_b_2");
    }
}
