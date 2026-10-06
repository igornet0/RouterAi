//! Provider and model capability discovery.

use serde::{Deserialize, Serialize};

/// Discrete capability that can be queried at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Chat completions.
    Chat,
    /// Responses-style API.
    Responses,
    /// Token streaming.
    Streaming,
    /// Embeddings.
    Embeddings,
    /// Image generation / understanding.
    Images,
    /// Audio.
    Audio,
    /// Moderation.
    Moderation,
    /// Account balance query.
    Balance,
    /// Usage / billing reports.
    Usage,
    /// Remote model listing.
    ModelList,
    /// Tool / function calling.
    ToolCalling,
    /// Structured JSON / schema output.
    StructuredOutput,
    /// Reasoning / thinking tokens.
    Reasoning,
    /// Prompt caching.
    PromptCaching,
}

/// Capability flags for a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Chat completions.
    pub chat: bool,
    /// Responses API.
    pub responses: bool,
    /// Streaming.
    pub streaming: bool,
    /// Embeddings.
    pub embeddings: bool,
    /// Images.
    pub images: bool,
    /// Audio.
    pub audio: bool,
    /// Moderation.
    pub moderation: bool,
    /// Balance endpoint.
    pub balance: bool,
    /// Usage reports.
    pub usage: bool,
    /// Model listing.
    pub model_list: bool,
    /// Prompt caching.
    pub prompt_caching: bool,
    /// Reasoning.
    pub reasoning: bool,
    /// Tool calling.
    pub tool_calling: bool,
    /// Structured output.
    pub structured_output: bool,
}

impl ProviderCapabilities {
    /// OpenAI-compatible chat + streaming baseline.
    pub fn openai_compatible_chat() -> Self {
        Self {
            chat: true,
            streaming: true,
            embeddings: true,
            model_list: true,
            tool_calling: true,
            structured_output: true,
            ..Default::default()
        }
    }

    /// Whether a discrete capability is supported.
    pub fn supports(&self, capability: Capability) -> bool {
        match capability {
            Capability::Chat => self.chat,
            Capability::Responses => self.responses,
            Capability::Streaming => self.streaming,
            Capability::Embeddings => self.embeddings,
            Capability::Images => self.images,
            Capability::Audio => self.audio,
            Capability::Moderation => self.moderation,
            Capability::Balance => self.balance,
            Capability::Usage => self.usage,
            Capability::ModelList => self.model_list,
            Capability::ToolCalling => self.tool_calling,
            Capability::StructuredOutput => self.structured_output,
            Capability::Reasoning => self.reasoning,
            Capability::PromptCaching => self.prompt_caching,
        }
    }
}

/// Per-model capability subset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Chat.
    pub chat: bool,
    /// Streaming.
    pub streaming: bool,
    /// Embeddings.
    pub embeddings: bool,
    /// Vision / image inputs.
    pub vision: bool,
    /// Tool calling.
    pub tool_calling: bool,
    /// Structured output.
    pub structured_output: bool,
    /// Reasoning.
    pub reasoning: bool,
    /// Prompt caching.
    pub prompt_caching: bool,
}

impl ModelCapabilities {
    /// Typical chat model.
    pub fn chat_default() -> Self {
        Self {
            chat: true,
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_balance_flag() {
        let mut caps = ProviderCapabilities::default();
        assert!(!caps.supports(Capability::Balance));
        caps.balance = true;
        assert!(caps.supports(Capability::Balance));
    }
}
