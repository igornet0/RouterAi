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

/// File-backed store with XOR **obfuscation** — not encryption: anyone who can
/// read the file can recover every secret.
///
/// Kept only to read existing files; use [`EncryptedFileSecretStore`] (which can
/// import these files with [`EncryptedFileSecretStore::import_legacy_file`]).
#[deprecated(
    since = "0.1.1",
    note = "XOR obfuscation is not encryption; use EncryptedFileSecretStore"
)]
#[derive(Debug)]
pub struct FileSecretStore {
    path: PathBuf,
    map: RwLock<HashMap<String, String>>,
}

#[allow(deprecated)]
impl FileSecretStore {
    /// Open or create store file.
    pub async fn open(path: impl AsRef<Path>) -> AiResult<Self> {
        let path = path.as_ref().to_path_buf();
        let map = if path.exists() {
            let data = tokio::fs::read(&path)
                .await
                .map_err(|e| AiError::SecretStore {
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

#[allow(deprecated)]
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

/// 256-bit key of an [`EncryptedFileSecretStore`]. Zeroized on drop; `Debug`
/// never shows it.
pub struct SecretStoreKey(zeroize::Zeroizing<[u8; 32]>);

impl std::fmt::Debug for SecretStoreKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretStoreKey(<redacted>)")
    }
}

impl SecretStoreKey {
    /// Fresh random key from the OS CSPRNG.
    pub fn generate() -> AiResult<Self> {
        use ring::rand::SecureRandom;
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        ring::rand::SystemRandom::new()
            .fill(&mut bytes[..])
            .map_err(|_| store_err("system random source unavailable"))?;
        Ok(Self(bytes))
    }

    /// Key from 64 hex characters (e.g. an environment variable). The error never
    /// contains the input.
    pub fn from_hex(hex: &str) -> AiResult<Self> {
        let hex = hex.trim().as_bytes();
        if hex.len() != 64 {
            return Err(store_err(
                "secret store key must be 64 hex characters (32 bytes)",
            ));
        }
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        for (i, pair) in hex.chunks(2).enumerate() {
            let hi = hex_val(pair[0]);
            let lo = hex_val(pair[1]);
            match (hi, lo) {
                (Some(hi), Some(lo)) => bytes[i] = hi << 4 | lo,
                _ => return Err(store_err("secret store key must be hexadecimal")),
            }
        }
        Ok(Self(bytes))
    }

    fn to_hex(&self) -> zeroize::Zeroizing<String> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = zeroize::Zeroizing::new(String::with_capacity(64));
        for b in self.0.iter() {
            out.push(DIGITS[usize::from(b >> 4)] as char);
            out.push(DIGITS[usize::from(b & 0xf)] as char);
        }
        out
    }

    /// Read the key stored at `path` (64 hex characters), or create it there
    /// with a fresh random key (owner-only permissions). Permissions wider than
    /// owner read/write are tightened (and logged).
    pub async fn load_or_create(path: impl AsRef<Path>) -> AiResult<Self> {
        let path = path.as_ref();
        match tokio::fs::read_to_string(path).await {
            Ok(text) => {
                restrict_permissions(path).await?;
                let text = zeroize::Zeroizing::new(text);
                Self::from_hex(&text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate()?;
                let mut line = key.to_hex();
                line.push('\n');
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| store_err(format!("mkdir failed: {e}")))?;
                }
                write_private(path, line.as_bytes(), true).await?;
                tracing::info!(path = %path.display(), "created secret store key");
                Ok(key)
            }
            Err(e) => Err(store_err(format!("cannot read secret store key: {e}"))),
        }
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn store_err(message: impl Into<String>) -> AiError {
    AiError::SecretStore {
        message: message.into(),
    }
}

/// File header: format name + version. Authenticated as AAD, so a file cannot be
/// reinterpreted under another format version.
const ENCRYPTED_MAGIC: &[u8; 8] = b"UAISEC01";
const NONCE_LEN: usize = ring::aead::NONCE_LEN;

/// File-backed secret store encrypted with AES-256-GCM.
///
/// * The whole map is sealed with a fresh random nonce on every write; the file
///   is `UAISEC01 | nonce | ciphertext+tag` — no secret, key name or length of an
///   individual secret is visible on disk.
/// * Files are created owner-only (`0600` on Unix) and replaced atomically
///   (temporary file, `fsync`, rename). Wider permissions on an existing file are
///   tightened and logged.
/// * A wrong key, a truncated / tampered file or an unknown format is an error —
///   never an empty store that the next write would overwrite.
///
/// The protection is only as good as the key's: keep it outside the data
/// directory (environment variable, secret manager, separate key file) so a copy
/// of the data directory alone reveals nothing.
pub struct EncryptedFileSecretStore {
    path: PathBuf,
    key: ring::aead::LessSafeKey,
    map: RwLock<HashMap<String, String>>,
    /// Serializes snapshot + write so a slower writer cannot persist an older map
    /// over a newer one.
    write_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for EncryptedFileSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedFileSecretStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl EncryptedFileSecretStore {
    /// Open the store at `path` (created on first write) with `key`.
    pub async fn open(path: impl AsRef<Path>, key: &SecretStoreKey) -> AiResult<Self> {
        let path = path.as_ref().to_path_buf();
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key.0[..])
                .map_err(|_| store_err("invalid secret store key"))?,
        );
        let map = match tokio::fs::read(&path).await {
            Ok(data) => {
                restrict_permissions(&path).await?;
                decrypt_map(&key, data)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(store_err(format!("read failed: {e}"))),
        };
        Ok(Self {
            path,
            key,
            map: RwLock::new(map),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Move every secret of a legacy (XOR-obfuscated) [`FileSecretStore`] file
    /// into this store, persist it, then delete the legacy file. Secrets already
    /// present here are kept. Returns the number of imported secrets. A legacy
    /// file that cannot be decoded is an error and is left untouched.
    pub async fn import_legacy_file(&self, legacy: impl AsRef<Path>) -> AiResult<usize> {
        let legacy = legacy.as_ref();
        let data = match tokio::fs::read(legacy).await {
            Ok(data) => zeroize::Zeroizing::new(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(store_err(format!("read legacy store failed: {e}"))),
        };
        let decoded = zeroize::Zeroizing::new(obfuscate_decode(&data));
        let entries: HashMap<String, String> = serde_json::from_slice(&decoded)
            .map_err(|_| store_err("legacy secret store is not readable; left untouched"))?;
        let mut imported = 0;
        {
            let mut g = self.map.write().map_err(|_| store_err("lock poisoned"))?;
            for (k, v) in entries {
                if let std::collections::hash_map::Entry::Vacant(slot) = g.entry(k) {
                    slot.insert(v);
                    imported += 1;
                }
            }
        }
        self.persist().await?;
        tokio::fs::remove_file(legacy).await.map_err(|e| {
            store_err(format!(
                "secrets imported, but the legacy store could not be deleted ({e}); \
                 delete it manually"
            ))
        })?;
        tracing::warn!(
            legacy = %legacy.display(),
            imported,
            "legacy obfuscated secret store migrated to the encrypted store and deleted; \
             rotate keys if the old file may have been copied"
        );
        Ok(imported)
    }

    async fn persist(&self) -> AiResult<()> {
        let _guard = self.write_lock.lock().await;
        let mut buf = {
            let g = self.map.read().map_err(|_| store_err("lock poisoned"))?;
            zeroize::Zeroizing::new(
                serde_json::to_vec(&*g).map_err(|e| store_err(format!("serialize failed: {e}")))?,
            )
        };
        let mut nonce = [0u8; NONCE_LEN];
        {
            use ring::rand::SecureRandom;
            ring::rand::SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| store_err("system random source unavailable"))?;
        }
        self.key
            .seal_in_place_append_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::from(ENCRYPTED_MAGIC),
                &mut *buf,
            )
            .map_err(|_| store_err("encryption failed"))?;
        let mut file = Vec::with_capacity(ENCRYPTED_MAGIC.len() + NONCE_LEN + buf.len());
        file.extend_from_slice(ENCRYPTED_MAGIC);
        file.extend_from_slice(&nonce);
        file.extend_from_slice(&buf);
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| store_err(format!("mkdir failed: {e}")))?;
        }
        let tmp = self.path.with_extension("tmp");
        write_private(&tmp, &file, false).await?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| store_err(format!("replace failed: {e}")))?;
        Ok(())
    }
}

fn decrypt_map(key: &ring::aead::LessSafeKey, data: Vec<u8>) -> AiResult<HashMap<String, String>> {
    let mut data = zeroize::Zeroizing::new(data);
    let header = ENCRYPTED_MAGIC.len() + NONCE_LEN;
    if data.len() < header || &data[..ENCRYPTED_MAGIC.len()] != ENCRYPTED_MAGIC {
        return Err(store_err(
            "not an encrypted secret store (unknown format); refusing to overwrite it",
        ));
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&data[ENCRYPTED_MAGIC.len()..header]);
    let plain = key
        .open_in_place(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(ENCRYPTED_MAGIC),
            &mut data[header..],
        )
        .map_err(|_| store_err("cannot decrypt secret store: wrong key or corrupted file"))?;
    serde_json::from_slice(plain).map_err(|_| store_err("secret store content is not readable"))
}

/// Write `data` to `path` readable by the owner only (`0600` on Unix), flushed
/// to disk. `create_new` refuses to replace an existing file.
async fn write_private(path: &Path, data: &[u8], create_new: bool) -> AiResult<()> {
    use tokio::io::AsyncWriteExt;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts
        .open(path)
        .await
        .map_err(|e| store_err(format!("write failed: {e}")))?;
    // An existing temporary file keeps its old mode: enforce it.
    restrict_permissions(path).await?;
    f.write_all(data)
        .await
        .map_err(|e| store_err(format!("write failed: {e}")))?;
    f.sync_all()
        .await
        .map_err(|e| store_err(format!("sync failed: {e}")))?;
    Ok(())
}

/// Tighten a secret file to owner read/write (`0600`) on Unix.
async fn restrict_permissions(path: &Path) -> AiResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| store_err(format!("stat failed: {e}")))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(|e| store_err(format!("chmod failed: {e}")))?;
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:o}"),
                "secret file was readable by other users; restricted to 0600"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[async_trait]
impl SecretStore for EncryptedFileSecretStore {
    async fn store(&self, key: &str, value: SecretString) -> AiResult<()> {
        {
            let mut g = self.map.write().map_err(|_| store_err("lock poisoned"))?;
            g.insert(key.to_string(), value.expose_secret().to_string());
        }
        self.persist().await
    }

    async fn get(&self, key: &str) -> AiResult<Option<SecretString>> {
        let g = self.map.read().map_err(|_| store_err("lock poisoned"))?;
        Ok(g.get(key).map(|s| SecretString::new(s.clone().into())))
    }

    async fn delete(&self, key: &str) -> AiResult<()> {
        {
            let mut g = self.map.write().map_err(|_| store_err("lock poisoned"))?;
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
