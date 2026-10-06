//! Built-in provider catalog for UI / HTTP discovery.

use serde::{Deserialize, Serialize};

use crate::error::{AiError, AiResult};
use crate::http::HttpClient;
use crate::provider::{DynProvider, ProviderCredential};
use crate::providers::{Anthropic, DeepSeek, Gemini, OpenAI, OpenAICompatible, OpenRouter};
use crate::secrets::SecretString;
use crate::types::ProviderId;
use std::sync::Arc;

/// Descriptor for a supported provider (or preset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// Stable id (`openai`, `deepseek`, `xai`, …).
    pub id: String,
    /// Human label.
    pub name: String,
    /// Default API base URL when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
    /// Whether the caller must supply a custom base URL.
    pub requires_base_url: bool,
    /// Short notes for the console.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Full catalog exposed to the console / API.
pub fn provider_catalog() -> Vec<ProviderDescriptor> {
    vec![
        ProviderDescriptor {
            id: "openai".into(),
            name: "OpenAI".into(),
            default_base_url: Some("https://api.openai.com/v1".into()),
            requires_base_url: false,
            notes: Some(
                "No prepaid balance via project keys — set a credit budget or use an Admin key for org costs"
                    .into(),
            ),
        },
        ProviderDescriptor {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            default_base_url: Some("https://api.deepseek.com".into()),
            requires_base_url: false,
            notes: Some("Includes /user/balance".into()),
        },
        ProviderDescriptor {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            default_base_url: Some("https://api.anthropic.com".into()),
            requires_base_url: false,
            notes: None,
        },
        ProviderDescriptor {
            id: "gemini".into(),
            name: "Google Gemini".into(),
            default_base_url: Some(
                "https://generativelanguage.googleapis.com/v1beta".into(),
            ),
            requires_base_url: false,
            notes: None,
        },
        ProviderDescriptor {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            default_base_url: Some("https://openrouter.ai/api/v1".into()),
            requires_base_url: false,
            notes: None,
        },
        ProviderDescriptor {
            id: "xai".into(),
            name: "xAI (Grok)".into(),
            default_base_url: Some("https://api.x.ai/v1".into()),
            requires_base_url: false,
            notes: Some("OpenAI-compatible preset".into()),
        },
        ProviderDescriptor {
            id: "openai-compatible".into(),
            name: "OpenAI-compatible".into(),
            default_base_url: None,
            requires_base_url: true,
            notes: Some("Ollama, vLLM, LM Studio, custom gateways".into()),
        },
    ]
}

/// Build a [`DynProvider`] from catalog id + API key (+ optional base URL).
///
/// The key is a static credential for standalone use. Managed keys should use
/// [`build_provider_template`] so every request binds the key selected for it.
pub fn build_provider(
    provider_id: &ProviderId,
    api_key: SecretString,
    base_url: Option<&str>,
    http: HttpClient,
) -> AiResult<DynProvider> {
    let template = build_provider_template(provider_id, base_url, http)?;
    template.with_credential(&ProviderCredential::new(api_key))
}

/// Build a keyless adapter for managed keys. It holds no credential: requests
/// fail unless [`crate::AiClient`] binds a selected key via
/// [`crate::Provider::with_credential`].
pub fn build_provider_template(
    provider_id: &ProviderId,
    base_url: Option<&str>,
    http: HttpClient,
) -> AiResult<DynProvider> {
    let id = provider_id.as_str();
    match id {
        "openai" => Ok(Arc::new(OpenAI::template(base_url, http)?)),
        "deepseek" => Ok(Arc::new(DeepSeek::template(base_url, http)?)),
        "anthropic" => Ok(Arc::new(Anthropic::template(base_url, http)?)),
        "gemini" => Ok(Arc::new(Gemini::template(base_url, http)?)),
        "openrouter" => Ok(Arc::new(OpenRouter::template(base_url, http)?)),
        "xai" | "grok" => Ok(Arc::new(
            OpenAICompatible::builder()
                .base_url(base_url.unwrap_or("https://api.x.ai/v1"))
                .provider_id(ProviderId::xai())
                .http(http)
                .build_unauthenticated()?,
        )),
        "openai-compatible" | "openai_compatible" => {
            let url = base_url.ok_or_else(|| AiError::InvalidRequest {
                message: "base_url is required for openai-compatible providers".into(),
            })?;
            Ok(Arc::new(
                OpenAICompatible::builder()
                    .base_url(url)
                    .provider_id(ProviderId::openai_compatible())
                    .http(http)
                    .build_unauthenticated()?,
            ))
        }
        other => {
            // Custom id with base URL → OpenAI-compatible under that provider id.
            let url = base_url.ok_or_else(|| AiError::InvalidRequest {
                message: format!(
                    "unknown provider '{other}'; supply base_url for a custom OpenAI-compatible endpoint"
                ),
            })?;
            Ok(Arc::new(
                OpenAICompatible::builder()
                    .base_url(url)
                    .provider_id(provider_id.clone())
                    .http(http)
                    .build_unauthenticated()?,
            ))
        }
    }
}
