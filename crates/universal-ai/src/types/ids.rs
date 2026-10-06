//! Strong identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AiError, AiResult};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            /// Create from any string-like value.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow inner string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// Provider identifier (e.g. `openai`, `deepseek`).
    ProviderId
);
string_id!(
    /// Model identifier within a provider.
    ModelId
);
string_id!(
    /// Account identifier.
    AccountId
);
string_id!(
    /// API key record identifier (not the secret itself).
    KeyId
);

impl ProviderId {
    /// OpenAI.
    pub fn openai() -> Self {
        Self::new("openai")
    }
    /// DeepSeek.
    pub fn deepseek() -> Self {
        Self::new("deepseek")
    }
    /// Anthropic.
    pub fn anthropic() -> Self {
        Self::new("anthropic")
    }
    /// Google Gemini.
    pub fn gemini() -> Self {
        Self::new("gemini")
    }
    /// OpenRouter.
    pub fn openrouter() -> Self {
        Self::new("openrouter")
    }
    /// Generic OpenAI-compatible.
    pub fn openai_compatible() -> Self {
        Self::new("openai-compatible")
    }
    /// xAI (Grok) — OpenAI-compatible preset.
    pub fn xai() -> Self {
        Self::new("xai")
    }
}

/// Internal request correlation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Generate a new random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RequestId {
    type Err = AiError;

    fn from_str(s: &str) -> AiResult<Self> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| AiError::InvalidRequest {
                message: format!("invalid request id: {e}"),
            })
    }
}

/// ISO currency code wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Currency(String);

impl Currency {
    /// USD.
    pub fn usd() -> Self {
        Self("USD".into())
    }

    /// Create from code.
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    /// Borrow code.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Currency {
    fn default() -> Self {
        Self::usd()
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
