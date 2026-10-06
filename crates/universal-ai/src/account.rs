//! Account and API key management.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::{AiError, AiResult};
use crate::router::KeySelectionStrategy;
use crate::secrets::{SecretStore, SecretString};
use crate::types::{AccountId, KeyId, ProviderId};
use crate::usage::UsageStatistics;

/// Account lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// Active.
    Active,
    /// Disabled by user.
    Disabled,
    /// Failed health/balance checks.
    Unhealthy,
}

/// Logical billing / credential account (keys hang off accounts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Id.
    pub id: AccountId,
    /// Provider.
    pub provider: ProviderId,
    /// Display name.
    pub name: String,
    /// Status.
    pub status: AccountStatus,
    /// Optional prepaid / credit budget set by the operator (USD).
    ///
    /// Used when the provider has no balance API (e.g. OpenAI project keys):
    /// estimated available = budget − locally tracked spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_budget: Option<rust_decimal::Decimal>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Last checked.
    pub last_checked_at: Option<DateTime<Utc>>,
}

/// Key status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    /// In rotation pool.
    Active,
    /// Temporarily disabled.
    Disabled,
    /// Superseded by rotation.
    Deprecated,
}

/// Metadata about a key — never contains the secret itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    /// Record id.
    pub id: KeyId,
    /// Provider.
    pub provider: ProviderId,
    /// Owning account.
    pub account_id: AccountId,
    /// Optional label.
    pub name: Option<String>,
    /// Optional base URL (openai-compatible / custom).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Last used.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Status.
    pub status: KeyStatus,
    /// Aggregated usage summary for this key.
    pub usage: UsageStatistics,
}

/// Input to register a key.
#[derive(Clone)]
pub struct AddKeyRequest {
    /// Provider.
    pub provider: ProviderId,
    /// Account.
    pub account_id: AccountId,
    /// Secret value.
    pub secret: SecretString,
    /// Optional name.
    pub name: Option<String>,
    /// Optional base URL for compatible / custom providers.
    pub base_url: Option<String>,
}

impl std::fmt::Debug for AddKeyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddKeyRequest")
            .field("provider", &self.provider)
            .field("account_id", &self.account_id)
            .field("secret", &"<redacted>")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Manages accounts.
#[derive(Clone, Default)]
pub struct AccountManager {
    accounts: Arc<RwLock<Vec<Account>>>,
}

impl AccountManager {
    /// Empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add account.
    pub async fn add_account(
        &self,
        provider: ProviderId,
        name: impl Into<String>,
    ) -> AiResult<Account> {
        let account = Account {
            id: AccountId::new(Uuid::new_v4().to_string()),
            provider,
            name: name.into(),
            status: AccountStatus::Active,
            credit_budget: None,
            created_at: Utc::now(),
            last_checked_at: None,
        };
        self.accounts.write().await.push(account.clone());
        Ok(account)
    }

    /// Remove by id.
    pub async fn remove_account(&self, id: &AccountId) -> AiResult<()> {
        let mut g = self.accounts.write().await;
        let before = g.len();
        g.retain(|a| &a.id != id);
        if g.len() == before {
            return Err(AiError::InvalidRequest {
                message: format!("account not found: {id}"),
            });
        }
        Ok(())
    }

    /// List all.
    pub async fn list_accounts(&self) -> Vec<Account> {
        self.accounts.read().await.clone()
    }

    /// Get one.
    pub async fn get(&self, id: &AccountId) -> Option<Account> {
        self.accounts.read().await.iter().find(|a| &a.id == id).cloned()
    }

    /// Mark checked.
    pub async fn check(&self, id: &AccountId) -> AiResult<Account> {
        let mut g = self.accounts.write().await;
        let account = g.iter_mut().find(|a| &a.id == id).ok_or_else(|| {
            AiError::InvalidRequest {
                message: format!("account not found: {id}"),
            }
        })?;
        account.last_checked_at = Some(Utc::now());
        Ok(account.clone())
    }

    /// Set operator-managed credit budget (USD decimal string / value).
    pub async fn set_credit_budget(
        &self,
        id: &AccountId,
        budget: Option<rust_decimal::Decimal>,
    ) -> AiResult<Account> {
        let mut g = self.accounts.write().await;
        let account = g.iter_mut().find(|a| &a.id == id).ok_or_else(|| {
            AiError::InvalidRequest {
                message: format!("account not found: {id}"),
            }
        })?;
        account.credit_budget = budget;
        Ok(account.clone())
    }

    /// Replace all accounts (used when loading from disk).
    pub async fn replace_all(&self, accounts: Vec<Account>) {
        *self.accounts.write().await = accounts;
    }

    /// Snapshot for persistence.
    pub async fn export(&self) -> Vec<Account> {
        self.list_accounts().await
    }
}

/// API key manager — secrets live only in [`SecretStore`].
pub struct ApiKeyManager {
    keys: RwLock<Vec<ApiKeyInfo>>,
    store: Arc<dyn SecretStore>,
    selection: RwLock<KeySelectionStrategy>,
    rr_counter: std::sync::atomic::AtomicU64,
}

impl ApiKeyManager {
    /// Create with a secret store backend.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            keys: RwLock::new(Vec::new()),
            store,
            selection: RwLock::new(KeySelectionStrategy::FirstAvailable),
            rr_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Set multi-key selection strategy.
    pub async fn set_selection_strategy(&self, strategy: KeySelectionStrategy) {
        *self.selection.write().await = strategy;
    }

    fn storage_key(id: &KeyId) -> String {
        format!("api_key:{id}")
    }

    /// Register a key (secret goes to store; metadata stays in memory/DB).
    pub async fn add_key(&self, req: AddKeyRequest) -> AiResult<ApiKeyInfo> {
        let info = ApiKeyInfo {
            id: KeyId::new(Uuid::new_v4().to_string()),
            provider: req.provider,
            account_id: req.account_id,
            name: req.name,
            base_url: req.base_url,
            created_at: Utc::now(),
            last_used_at: None,
            status: KeyStatus::Active,
            usage: UsageStatistics::default(),
        };
        self.store
            .store(&Self::storage_key(&info.id), req.secret)
            .await?;
        self.keys.write().await.push(info.clone());
        // Never log the secret — only the key record id.
        tracing::info!(key_id = %info.id, provider = %info.provider, "api key added");
        Ok(info)
    }

    /// Remove metadata + secret.
    pub async fn remove_key(&self, id: &KeyId) -> AiResult<()> {
        self.store.delete(&Self::storage_key(id)).await?;
        let mut g = self.keys.write().await;
        let before = g.len();
        g.retain(|k| &k.id != id);
        if g.len() == before {
            return Err(AiError::InvalidRequest {
                message: format!("key not found: {id}"),
            });
        }
        Ok(())
    }

    /// List metadata (no secrets).
    pub async fn list_keys(&self) -> Vec<ApiKeyInfo> {
        self.keys.read().await.clone()
    }

    /// Get metadata.
    pub async fn get_key(&self, id: &KeyId) -> Option<ApiKeyInfo> {
        self.keys.read().await.iter().find(|k| &k.id == id).cloned()
    }

    /// Fetch secret for outbound HTTP (caller must not log it).
    pub async fn get_secret(&self, id: &KeyId) -> AiResult<Option<SecretString>> {
        self.store.get(&Self::storage_key(id)).await
    }

    /// Disable key.
    pub async fn disable_key(&self, id: &KeyId) -> AiResult<()> {
        self.set_status(id, KeyStatus::Disabled).await
    }

    /// Enable key.
    pub async fn enable_key(&self, id: &KeyId) -> AiResult<()> {
        self.set_status(id, KeyStatus::Active).await
    }

    async fn set_status(&self, id: &KeyId, status: KeyStatus) -> AiResult<()> {
        let mut g = self.keys.write().await;
        let key = g.iter_mut().find(|k| &k.id == id).ok_or_else(|| {
            AiError::InvalidRequest {
                message: format!("key not found: {id}"),
            }
        })?;
        key.status = status;
        Ok(())
    }

    /// Manual rotation: store new secret, deprecate old, activate new.
    pub async fn rotate_key(&self, id: &KeyId, new_secret: SecretString) -> AiResult<ApiKeyInfo> {
        let old = self.get_key(id).await.ok_or_else(|| AiError::InvalidRequest {
            message: format!("key not found: {id}"),
        })?;
        let new_info = self
            .add_key(AddKeyRequest {
                provider: old.provider.clone(),
                account_id: old.account_id.clone(),
                secret: new_secret,
                name: old.name.clone(),
                base_url: old.base_url.clone(),
            })
            .await?;
        self.set_status(id, KeyStatus::Deprecated).await?;
        tracing::info!(old_key = %id, new_key = %new_info.id, "api key rotated");
        Ok(new_info)
    }

    /// Select a key for a provider using the configured strategy.
    pub async fn select_key(&self, provider: &ProviderId) -> AiResult<Option<ApiKeyInfo>> {
        let keys: Vec<_> = self
            .keys
            .read()
            .await
            .iter()
            .filter(|k| &k.provider == provider && k.status == KeyStatus::Active)
            .cloned()
            .collect();
        if keys.is_empty() {
            return Ok(None);
        }
        let strategy = *self.selection.read().await;
        let chosen = match strategy {
            KeySelectionStrategy::FirstAvailable => keys.into_iter().next(),
            KeySelectionStrategy::RoundRobin => {
                let i = self.rr_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let len = keys.len().max(1);
                keys.into_iter().nth((i as usize) % len)
            }
            KeySelectionStrategy::LeastUsed => keys
                .into_iter()
                .min_by_key(|k| k.usage.requests),
            KeySelectionStrategy::LowestCost => keys
                .into_iter()
                .min_by(|a, b| a.usage.total_cost.cmp(&b.usage.total_cost)),
            KeySelectionStrategy::HighestBalance => keys.into_iter().next(),
        };
        Ok(chosen)
    }

    /// Mark last used.
    pub async fn touch(&self, id: &KeyId) {
        if let Some(k) = self.keys.write().await.iter_mut().find(|k| &k.id == id) {
            k.last_used_at = Some(Utc::now());
        }
    }

    /// Replace metadata (secrets must already exist in the store).
    pub async fn replace_metadata(&self, keys: Vec<ApiKeyInfo>) {
        *self.keys.write().await = keys;
    }

    /// Snapshot metadata for persistence (no secrets).
    pub async fn export_metadata(&self) -> Vec<ApiKeyInfo> {
        self.list_keys().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretStore;
    use secrecy::ExposeSecret;

    #[tokio::test]
    async fn key_manager_does_not_debug_secret() {
        let store = Arc::new(MemorySecretStore::new());
        let mgr = ApiKeyManager::new(store);
        let account = AccountId::new("acc");
        let req = AddKeyRequest {
            provider: ProviderId::deepseek(),
            account_id: account,
            secret: SecretString::new("sk-should-not-appear".into()),
            name: Some("main".into()),
            base_url: None,
        };
        let dbg = format!("{req:?}");
        assert!(!dbg.contains("sk-should-not-appear"));
        let info = mgr.add_key(req).await.unwrap();
        let secret = mgr.get_secret(&info.id).await.unwrap().unwrap();
        assert_eq!(secret.expose_secret(), "sk-should-not-appear");
    }
}
