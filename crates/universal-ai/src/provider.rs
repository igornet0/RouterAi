//! Provider trait — adapters implement this; capabilities gate optional methods.

use async_trait::async_trait;

use crate::balance::Balance;
use crate::capability::{Capability, ProviderCapabilities};
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::models::ModelInfo;
use crate::types::{ChatRequest, ChatResponse, ChatStream, ProviderId};
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
}

/// Optional provider-specific surface.
pub trait ProviderExtensions: Send + Sync {
    /// Opaque name (e.g. `deepseek`, `anthropic`).
    fn name(&self) -> &'static str;
}

/// Object-safe wrapper stored in AiClient.
pub type DynProvider = std::sync::Arc<dyn Provider>;
