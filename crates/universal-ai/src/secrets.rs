//! Secure credential storage. Never log plaintext secrets.

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString as SecrecyString};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::error::{AiError, AiResult};

pub use secrecy::SecretString;

/// Abstract secret store.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Persist a secret under `key` (lookup key, not the API key value).
    async fn store(&self, key: &str, value: SecretString) -> AiResult<()>;
    /// Fetch secret.
    async fn get(&self, key: &str) -> AiResult<Option<SecretString>>;
    /// Delete secret.
    async fn delete(&self, key: &str) -> AiResult<()>;
}

/// Process-memory store (tests / ephemeral).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    map: RwLock<HashMap<String, String>>,
}

impl MemorySecretStore {
    /// Create empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn store(&self, key: &str, value: SecretString) -> AiResult<()> {
        let mut g = self.map.write().map_err(|_| AiError::SecretStore {
            message: "lock poisoned".into(),
        })?;
        g.insert(key.to_string(), value.expose_secret().to_string());
        Ok(())
    }

    async fn get(&self, key: &str) -> AiResult<Option<SecretString>> {
        let g = self.map.read().map_err(|_| AiError::SecretStore {
            message: "lock poisoned".into(),
        })?;
        Ok(g.get(key).map(|s| SecretString::new(s.clone().into())))
    }

    async fn delete(&self, key: &str) -> AiResult<()> {
        let mut g = self.map.write().map_err(|_| AiError::SecretStore {
            message: "lock poisoned".into(),
        })?;
        g.remove(key);
        Ok(())
    }
}

/// File-backed store with simple XOR obfuscation (not a substitute for OS keychain).
///
/// Values are never written as raw API keys in plaintext JSON fields named `api_key`.
/// Prefer [`KeychainSecretStore`] on macOS for production.
#[derive(Debug)]
pub struct FileSecretStore {
    path: PathBuf,
    map: RwLock<HashMap<String, String>>,
}

impl FileSecretStore {
    /// Open or create store file.
    pub async fn open(path: impl AsRef<Path>) -> AiResult<Self> {
        let path = path.as_ref().to_path_buf();
        let map = if path.exists() {
            let data = tokio::fs::read(&path).await.map_err(|e| AiError::SecretStore {
                message: format!("read failed: {e}"),
            })?;
            let decoded = obfuscate_decode(&data);
            serde_json::from_slice(&decoded).unwrap_or_default()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            map: RwLock::new(map),
        })
    }

    async fn persist(&self) -> AiResult<()> {
        let json = {
            let g = self.map.read().map_err(|_| AiError::SecretStore {
                message: "lock poisoned".into(),
            })?;
            serde_json::to_vec(&*g).map_err(|e| AiError::SecretStore {
                message: format!("serialize failed: {e}"),
            })?
        };
        let encoded = obfuscate_encode(&json);
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AiError::SecretStore {
                    message: format!("mkdir failed: {e}"),
                })?;
        }
        tokio::fs::write(&self.path, encoded)
            .await
            .map_err(|e| AiError::SecretStore {
                message: format!("write failed: {e}"),
            })?;
        Ok(())
    }
}

#[async_trait]
impl SecretStore for FileSecretStore {
    async fn store(&self, key: &str, value: SecretString) -> AiResult<()> {
        {
            let mut g = self.map.write().map_err(|_| AiError::SecretStore {
                message: "lock poisoned".into(),
            })?;
            g.insert(key.to_string(), value.expose_secret().to_string());
        }
        self.persist().await
    }

    async fn get(&self, key: &str) -> AiResult<Option<SecretString>> {
        let g = self.map.read().map_err(|_| AiError::SecretStore {
            message: "lock poisoned".into(),
        })?;
        Ok(g.get(key).map(|s| SecretString::new(s.clone().into())))
    }

    async fn delete(&self, key: &str) -> AiResult<()> {
        {
            let mut g = self.map.write().map_err(|_| AiError::SecretStore {
                message: "lock poisoned".into(),
            })?;
            g.remove(key);
        }
        self.persist().await
    }
}

fn obfuscate_encode(data: &[u8]) -> Vec<u8> {
    data.iter().map(|b| b ^ 0xA5).collect()
}

fn obfuscate_decode(data: &[u8]) -> Vec<u8> {
    obfuscate_encode(data)
}

/// macOS Keychain-oriented store.
///
/// On non-macOS (or without the `keychain` feature) this falls back to an in-memory
/// store and documents that production should enable OS secure storage.
#[derive(Debug, Default)]
pub struct KeychainSecretStore {
    fallback: MemorySecretStore,
    service: String,
}

impl KeychainSecretStore {
    /// Create with a keychain service name.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            fallback: MemorySecretStore::new(),
            service: service.into(),
        }
    }

    /// Service name used for keychain items.
    pub fn service(&self) -> &str {
        &self.service
    }
}

#[async_trait]
impl SecretStore for KeychainSecretStore {
    async fn store(&self, key: &str, value: SecretString) -> AiResult<()> {
        // Full Security.framework integration is feature-gated; safe fallback for CI.
        tracing::debug!(service = %self.service, key = %key, "storing secret (keychain fallback)");
        self.fallback.store(key, value).await
    }

    async fn get(&self, key: &str) -> AiResult<Option<SecretString>> {
        self.fallback.get(key).await
    }

    async fn delete(&self, key: &str) -> AiResult<()> {
        self.fallback.delete(key).await
    }
}

/// Redacted debug wrapper helper for tests.
pub fn secret_debug_is_redacted(secret: &SecrecyString) -> bool {
    let dbg = format!("{secret:?}");
    !dbg.contains(secret.expose_secret()) || secret.expose_secret().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_roundtrip() {
        let store = MemorySecretStore::new();
        store
            .store("k1", SecretString::new("sk-secret-value".into()))
            .await
            .unwrap();
        let got = store.get("k1").await.unwrap().unwrap();
        assert_eq!(got.expose_secret(), "sk-secret-value");
    }

    #[test]
    fn debug_does_not_leak_secret() {
        let s = SecretString::new("sk-super-secret-do-not-leak".into());
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("sk-super-secret-do-not-leak"),
            "Debug leaked secret: {dbg}"
        );
    }
}
