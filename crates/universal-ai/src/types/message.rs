//! Messages and multimodal content.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Chat role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System instructions.
    System,
    /// End-user.
    User,
    /// Model.
    Assistant,
    /// Tool result.
    Tool,
}

/// Reference to an uploaded / remote file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReference {
    /// File id or path.
    pub id: String,
    /// Optional MIME type.
    pub mime_type: Option<String>,
    /// Optional display name.
    pub name: Option<String>,
}

/// Multimodal content part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text {
        /// Text body.
        text: String,
    },
    /// Image by URL.
    ImageUrl {
        /// Image URL.
        url: String,
    },
    /// Audio by URL.
    AudioUrl {
        /// Audio URL.
        url: String,
    },
    /// File reference.
    File {
        /// File metadata.
        file: FileReference,
    },
}

impl ContentPart {
    /// Convenience text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Message content: plain string or multimodal parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// Single text blob.
    Text(String),
    /// Multimodal parts.
    Parts(Vec<ContentPart>),
}

impl Content {
    /// Borrow as plain text when possible.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            Self::Parts(parts) => {
                if let [ContentPart::Text { text }] = parts.as_slice() {
                    Some(text)
                } else {
                    None
                }
            }
        }
    }

    /// Flatten text parts (lossy for non-text).
    pub fn to_plain_text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl From<&str> for Content {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Content {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

/// A single chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Role.
    pub role: Role,
    /// Content.
    pub content: Content,
    /// Optional name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool call id when role is Tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Assistant tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool result reports a failed execution (role Tool only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl Message {
    /// System message.
    pub fn system(content: impl Into<Content>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    /// User message.
    pub fn user(content: impl Into<Content>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    /// Assistant message.
    pub fn assistant(content: impl Into<Content>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    /// Tool result message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<Content>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            name: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    /// Assistant message requesting tool calls (content may be empty).
    pub fn assistant_tool_calls(content: impl Into<Content>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::assistant(content)
        }
    }

    /// Tool result message (success or failure).
    pub fn tool_result(result: ToolResult) -> Self {
        Self {
            is_error: result.is_error,
            ..Self::tool(result.tool_call_id, result.content)
        }
    }
}

impl From<ToolResult> for Message {
    fn from(value: ToolResult) -> Self {
        Self::tool_result(value)
    }
}

/// Outcome of executing a [`ToolCall`], sent back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the call this result answers.
    pub tool_call_id: String,
    /// Result payload (usually JSON text).
    pub content: String,
    /// Execution failed; content describes the error.
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    /// Successful result.
    pub fn success(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// Failed result (the model sees the error and may retry).
    pub fn error(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            is_error: true,
        }
    }
}

/// Tool definition for function calling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// Tool type (usually `function`).
    #[serde(rename = "type")]
    pub tool_type: String,
    /// Function schema.
    pub function: ToolFunction,
}

impl Tool {
    /// Create a function tool.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            tool_type: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: Some(description.into()),
                parameters,
            },
        }
    }
}

impl Tool {
    /// Function name as seen by the model.
    pub fn name(&self) -> &str {
        &self.function.name
    }
}

/// Function tool schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFunction {
    /// Function name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema parameters.
    pub parameters: serde_json::Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Call id.
    pub id: String,
    /// Type (usually function).
    #[serde(rename = "type")]
    pub call_type: String,
    /// Function invocation.
    pub function: FunctionCall,
}

impl ToolCall {
    /// Function call with JSON arguments.
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: &serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// Function name requested by the model.
    pub fn name(&self) -> &str {
        &self.function.name
    }

    /// Parse arguments as JSON (empty string → `{}`).
    pub fn arguments_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        let raw = self.function.arguments.trim();
        if raw.is_empty() {
            return Ok(serde_json::Value::Object(Default::default()));
        }
        serde_json::from_str(raw)
    }
}

/// Function name + arguments JSON string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name.
    pub name: String,
    /// Arguments as JSON string.
    pub arguments: String,
}

/// Desired response format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free text.
    Text,
    /// JSON object.
    JsonObject,
    /// JSON schema.
    JsonSchema {
        /// Schema name.
        name: String,
        /// Schema document.
        schema: serde_json::Value,
        /// Strict mode when supported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Arbitrary string metadata bag.
pub type Metadata = HashMap<String, String>;
