//! Persist account / key metadata next to the secret store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use universal_ai::{
    Account, AiClient, ApiKeyInfo, EncryptedFileSecretStore, KeyStatus, SecretStoreKey,
    SqliteStorage,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialsFile {
    pub accounts: Vec<Account>,
    pub keys: Vec<ApiKeyInfo>,
}

/// Data directory for secrets + metadata.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ROUTERAI_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs_fallback()
}

fn dirs_fallback() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".routerai");
    }
    PathBuf::from(".routerai")
}

/// Encrypted secret store (AES-256-GCM, owner-only file).
pub fn secrets_path(dir: &Path) -> PathBuf {
    dir.join("secrets.enc")
}

/// Legacy XOR-obfuscated store: imported into [`secrets_path`] and deleted on start.
pub fn legacy_secrets_path(dir: &Path) -> PathBuf {
    dir.join("secrets.bin")
}

/// Fallback location of the secret store key when none is configured.
pub fn default_secrets_key_path(dir: &Path) -> PathBuf {
    dir.join("secrets.key")
}

/// Key of the secret store: `ROUTERAI_SECRETS_KEY` (64 hex characters), else the
/// key file named by `ROUTERAI_SECRETS_KEY_FILE`, else `<data dir>/secrets.key`
/// (created on first start). Keep the key outside the data directory in
/// production: a copy of the data directory that includes its key reveals the
/// secrets.
pub async fn secret_store_key(dir: &Path) -> Result<SecretStoreKey, Box<dyn std::error::Error>> {
    if let Ok(hex) = std::env::var("ROUTERAI_SECRETS_KEY") {
        if !hex.trim().is_empty() {
            return Ok(SecretStoreKey::from_hex(&hex)?);
        }
    }
    if let Some(path) = std::env::var_os("ROUTERAI_SECRETS_KEY_FILE") {
        return Ok(SecretStoreKey::load_or_create(PathBuf::from(path)).await?);
    }
    let path = default_secrets_key_path(dir);
    tracing::warn!(
        key_file = %path.display(),
        "secret store key is kept inside the data directory; set ROUTERAI_SECRETS_KEY or \
         ROUTERAI_SECRETS_KEY_FILE (outside the data directory) for production"
    );
    Ok(SecretStoreKey::load_or_create(path).await?)
}

pub fn credentials_path(dir: &Path) -> PathBuf {
    dir.join("credentials.json")
}

/// SQLite path for RouterAi agents / events / runs / handlers / schedules.
pub fn routerai_db_path(dir: &Path) -> PathBuf {
    dir.join("routerai.db")
}

pub async fn load_credentials(path: &Path) -> CredentialsFile {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => CredentialsFile::default(),
    }
}

pub async fn save_credentials(path: &Path, file: &CredentialsFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| e.to_string())
}

pub async fn persist_from_client(ai: &AiClient, path: &Path) -> Result<(), String> {
    let file = CredentialsFile {
        accounts: ai.accounts().export().await,
        keys: ai.keys().export_metadata().await,
    };
    save_credentials(path, &file).await
}

/// Build AiClient with the encrypted secret store (migrating a legacy obfuscated
/// store) and restore providers from persisted keys.
pub async fn build_ai_client(
    dir: &Path,
    secrets_key: &SecretStoreKey,
) -> Result<Arc<AiClient>, Box<dyn std::error::Error>> {
    tokio::fs::create_dir_all(dir).await?;
    let secret_store = EncryptedFileSecretStore::open(secrets_path(dir), secrets_key).await?;
    secret_store
        .import_legacy_file(legacy_secrets_path(dir))
        .await?;
    let secret_store = Arc::new(secret_store);
    let db_path = dir.join("universal-ai.db");
    let storage = Arc::new(
        SqliteStorage::connect(&format!("sqlite://{}?mode=rwc", db_path.display())).await?,
    );
    let ai = Arc::new(
        AiClient::builder()
            .allow_empty_providers()
            .secret_store(secret_store)
            .storage(storage)
            .with_example_prices()
            .build()?,
    );

    let creds = load_credentials(&credentials_path(dir)).await;
    ai.accounts().replace_all(creds.accounts).await;
    ai.keys().replace_metadata(creds.keys.clone()).await;

    for info in creds.keys {
        if info.status != KeyStatus::Active {
            continue;
        }
        if let Err(err) = ai
            .sync_provider_from_key(&info, info.base_url.as_deref())
            .await
        {
            tracing::warn!(
                key_id = %info.id,
                provider = %info.provider,
                error = %err,
                "failed to restore provider from key"
            );
        }
    }

    Ok(ai)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use universal_ai::{KeyId, SecretStore, SecretString};

    const CREDENTIAL: &str = "TEST-CREDENTIAL-PLAINTEXT-SERVER-0123456789";

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Start-up on a data directory written by an older server: the obfuscated
    /// store is migrated into the encrypted one and deleted, no plaintext key is
    /// left on disk, and secret files are owner-only.
    #[tokio::test]
    #[allow(deprecated)]
    async fn startup_migrates_secrets() {
        let dir = std::env::temp_dir().join(format!("routerai-server-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        {
            let legacy = universal_ai::FileSecretStore::open(legacy_secrets_path(&dir))
                .await
                .unwrap();
            legacy
                .store("api_key:k1", SecretString::new(CREDENTIAL.into()))
                .await
                .unwrap();
        }

        let key = SecretStoreKey::load_or_create(default_secrets_key_path(&dir))
            .await
            .unwrap();
        let ai = build_ai_client(&dir, &key).await.unwrap();

        let secret = ai
            .keys()
            .get_secret(&KeyId::new("k1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(secret.expose_secret(), CREDENTIAL);
        assert!(!legacy_secrets_path(&dir).exists(), "legacy store deleted");

        // No file in the data directory holds the credential in plaintext.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let raw = std::fs::read(&path).unwrap();
                assert!(
                    !contains(&raw, CREDENTIAL.as_bytes()),
                    "plaintext credential in {}",
                    path.display()
                );
            }
        }
        #[cfg(unix)]
        for path in [secrets_path(&dir), default_secrets_key_path(&dir)] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }
}
