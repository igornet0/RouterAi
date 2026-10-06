//! Provider adapters. OpenAI-compatible HTTP is one adapter family — not the core model.

mod anthropic;
mod deepseek;
mod gemini;
mod openai;
mod openai_compatible;
mod openrouter;

pub use anthropic::Anthropic;
pub use deepseek::DeepSeek;
pub use gemini::Gemini;
pub use openai::OpenAI;
pub use openai_compatible::{OpenAICompatible, OpenAICompatibleBuilder};
pub use openrouter::OpenRouter;
