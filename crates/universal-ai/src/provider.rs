//! Provider trait — adapters implement this; capabilities gate optional methods.

use async_trait::async_trait;
use secrecy::SecretString;

use crate::balance::Balance;
use crate::capability::{Capability, ProviderCapabilities};
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::models::ModelInfo;
use crate::types::{ChatRequest, ChatResponse, ChatStream, KeyId, ProviderId};
use crate::usage::{UsageReport, UsageRequest};

/// Core provider adapter interface.
///
/// Do not force every method on every provider — check [`Provider::supports`] first.
/// Unsupported operations should return [`AiError::UnsupportedCapability`].
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider id.
    fn id(&self) -> ProviderId;

    /// Advertised capabilities.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Capability check helper.
    fn supports(&self, capability: Capability) -> bool {
        self.capabilities().supports(capability)
    }

    /// List models when supported.
    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        Err(AiError::UnsupportedCapability {
            capability: Capability::ModelList,
            provider: Some(self.id()),
        })
    }

    /// Chat completion.
    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let _ = request;
        Err(AiError::UnsupportedCapability {
            capability: Capability::Chat,
            provider: Some(self.id()),
        })
    }

    /// Streaming chat.
    async fn stream_chat(&self, request: ChatRequest) -> AiResult<ChatStream> {
        let _ = request;
        Err(AiError::UnsupportedCapability {
            capability: Capability::Streaming,
            provider: Some(self.id()),
        })
    }

    /// Account balance when the provider exposes it. `Ok(None)` if unknown — never invent.
    async fn balance(&self) -> AiResult<Option<Balance>> {
        if !self.supports(Capability::Balance) {
            return Err(AiError::UnsupportedCapability {
                capability: Capability::Balance,
                provider: Some(self.id()),
            });
        }
        Ok(None)
    }

    /// Usage report when supported.
    async fn usage(&self, request: UsageRequest) -> AiResult<UsageReport> {
        let _ = request;
        Err(AiError::UnsupportedCapability {
            capability: Capability::Usage,
            provider: Some(self.id()),
        })
    }

    /// Lightweight health probe.
    async fn health(&self) -> AiResult<HealthStatus>;

    /// Escape hatch for provider-specific JSON / raw ops.
    fn extensions(&self) -> Option<&dyn ProviderExtensions> {
        None
    }

    /// A copy of this adapter that authenticates every call with `credential` only.
    ///
    /// [`crate::AiClient`] calls this once per provider attempt with the API key it
    /// selected, so the credential belongs to that request: the receiver is never
    /// modified and concurrent requests never share a bound instance. Adapters that
    /// cannot accept per-request credentials return an error instead of silently
    /// using another key.
    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        let _ = credential;
        Err(AiError::Config {
            message: format!("provider {} does not support managed API keys", self.id()),
        })
    }
}

/// API credential bound to one provider request (usually a managed key).
#[derive(Clone)]
pub struct ProviderCredential {
    key_id: Option<KeyId>,
    secret: SecretString,
    base_url: Option<String>,
}

impl ProviderCredential {
    /// Ad-hoc credential not tracked by the key manager.
    pub fn new(secret: SecretString) -> Self {
        Self {
            key_id: None,
            secret,
            base_url: None,
        }
    }

    /// Managed key; `base_url` (when set) overrides the adapter endpoint so the
    /// secret is only sent to the endpoint it was registered for.
    pub fn for_key(key_id: KeyId, secret: SecretString, base_url: Option<String>) -> Self {
        Self {
            key_id: Some(key_id),
            secret,
            base_url: base_url.filter(|u| !u.trim().is_empty()),
        }
    }

    /// Managed key record id, if any.
    pub fn key_id(&self) -> Option<&KeyId> {
        self.key_id.as_ref()
    }

    /// Secret value (callers must not log it).
    pub fn secret(&self) -> &SecretString {
        &self.secret
    }

    /// Endpoint override for this key.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }
}

impl std::fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("key_id", &self.key_id)
            .field("secret", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Optional provider-specific surface.
pub trait ProviderExtensions: Send + Sync {
    /// Opaque name (e.g. `deepseek`, `anthropic`).
    fn name(&self) -> &'static str;
}

/// Object-safe wrapper stored in AiClient.
pub type DynProvider = std::sync::Arc<dyn Provider>;
