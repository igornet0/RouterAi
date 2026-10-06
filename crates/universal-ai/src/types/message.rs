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
