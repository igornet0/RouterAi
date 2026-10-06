//! Persist account / key metadata next to the secret store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use universal_ai::{Account, AiClient, ApiKeyInfo, FileSecretStore, KeyStatus, SqliteStorage};

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

pub fn secrets_path(dir: &Path) -> PathBuf {
    dir.join("secrets.bin")
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

/// Build AiClient with file secrets and restore providers from persisted keys.
pub async fn build_ai_client(dir: &Path) -> Result<Arc<AiClient>, Box<dyn std::error::Error>> {
    tokio::fs::create_dir_all(dir).await?;
    let secret_store = Arc::new(FileSecretStore::open(secrets_path(dir)).await?);
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
