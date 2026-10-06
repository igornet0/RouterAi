//! Streaming events.

use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

use crate::error::AiError;
use crate::types::message::ToolCall;
use crate::usage::Usage;

/// Stream event from a chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Incremental text.
    TextDelta {
        /// Delta text.
        text: String,
    },
    /// Tool call fragment or complete call.
    ToolCall {
        /// Tool call payload.
        call: ToolCall,
    },
    /// Usage reported at end (or mid-stream for some providers).
    Usage {
        /// Usage numbers.
        usage: Usage,
    },
    /// Stream completed successfully.
    Done,
}

/// Boxed async stream of chat events.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, AiError>> + Send>>;
