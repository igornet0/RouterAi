//! Chat request / response canonical types.

use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::types::ids::{ModelId, RequestId};
use crate::types::message::{Message, Metadata, ResponseFormat, Tool, ToolCall};
use crate::usage::Usage;

/// Canonical chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Target model.
    pub model: ModelId,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Max completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Response format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// User-defined metadata.
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
    /// Request may have side effects (disables automatic fallback).
    #[serde(default)]
    pub side_effecting: bool,
    /// Prefer this provider when resolving (e.g. agent model.provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_provider: Option<crate::types::ProviderId>,
}

impl ChatRequest {
    /// Build a simple single-user-message request.
    pub fn simple(model: impl Into<ModelId>, message: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![Message::user(message.into())],
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: Vec::new(),
            tools: Vec::new(),
            response_format: None,
            metadata: Metadata::new(),
            side_effecting: false,
            preferred_provider: None,
        }
    }
}

/// Finish reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Hit token limit.
    Length,
    /// Tool call.
    ToolCalls,
    /// Content filter.
    ContentFilter,
    /// Other / unknown.
    Other(String),
}

/// Canonical chat response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Correlation id.
    pub request_id: RequestId,
    /// Model used.
    pub model: ModelId,
    /// Assistant message.
    pub message: Message,
    /// Finish reason.
    pub finish_reason: Option<FinishReason>,
    /// Token usage when reported.
    pub usage: Option<Usage>,
    /// Calculated cost when pricing available.
    pub cost: Option<Cost>,
    /// Raw provider payload for escape hatch (sanitized / optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl ChatResponse {
    /// Assistant text helper.
    pub fn text(&self) -> String {
        self.message.content.to_plain_text()
    }

    /// Optional cost amount display.
    pub fn cost(&self) -> Option<&Cost> {
        self.cost.as_ref()
    }

    /// Optional usage.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// Tool calls on the assistant message.
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.message.tool_calls
    }
}
