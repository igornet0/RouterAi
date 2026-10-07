//! `EncryptedFileSecretStore`: nothing readable on disk, owner-only files, and
//! fail-closed on a wrong key or a damaged file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::ExposeSecret;
use universal_ai::{EncryptedFileSecretStore, SecretStore, SecretStoreKey, SecretString};

/// Stand-in credential (not a real key format).
const API_KEY: &str = "TEST-CREDENTIAL-PLAINTEXT-MUST-NOT-HIT-DISK-0123456789";

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("uai-secrets-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn secret(s: &str) -> SecretString {
    SecretString::new(s.to_string().into())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn secrets_round_trip_and_never_hit_disk_in_plaintext() {
    let dir = temp_dir();
    let (path, key_path) = (dir.join("secrets.enc"), dir.join("secrets.key"));
    let key = SecretStoreKey::load_or_create(&key_path).await.unwrap();
    let store = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    store.store("key_openai_1", secret(API_KEY)).await.unwrap();
    store
        .store("key_other", secret("second-secret"))
        .await
        .unwrap();

    let raw = std::fs::read(&path).unwrap();
    assert!(
        !contains(&raw, API_KEY.as_bytes()),
        "plaintext API key on disk"
    );
    assert!(
        !contains(&raw, b"key_openai_1"),
        "key names are encrypted too"
    );
    // Not merely obfuscated: no single-byte XOR of the secret appears either.
    for x in 1..=255u8 {
        let xored: Vec<u8> = API_KEY.bytes().map(|b| b ^ x).collect();
        assert!(
            !contains(&raw, &xored),
            "secret recoverable with XOR {x:#x}"
        );
    }
    #[cfg(unix)]
    {
        assert_eq!(mode(&path), 0o600, "store file owner-only");
        assert_eq!(mode(&key_path), 0o600, "key file owner-only");
    }

    // Reopen with the same key file.
    let key = SecretStoreKey::load_or_create(&key_path).await.unwrap();
    let reopened = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    let got = reopened.get("key_openai_1").await.unwrap().unwrap();
    assert_eq!(got.expose_secret(), API_KEY);
    reopened.delete("key_other").await.unwrap();
    let again = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    assert!(again.get("key_other").await.unwrap().is_none());
    assert!(again.get("key_openai_1").await.unwrap().is_some());
    // Debug shows neither the key nor the secrets.
    let dbg = format!("{again:?} {key:?}");
    assert!(!dbg.contains(API_KEY) && dbg.contains("redacted"), "{dbg}");
}

#[tokio::test]
async fn wrong_key_or_damaged_file_is_an_error_not_an_empty_store() {
    let dir = temp_dir();
    let path = dir.join("secrets.enc");
    let key = SecretStoreKey::generate().unwrap();
    EncryptedFileSecretStore::open(&path, &key)
        .await
        .unwrap()
        .store("k", secret(API_KEY))
        .await
        .unwrap();

    let other = SecretStoreKey::generate().unwrap();
    let err = EncryptedFileSecretStore::open(&path, &other)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("secret store"), "{err}");

    // One flipped ciphertext bit is detected (authenticated encryption).
    let mut raw = std::fs::read(&path).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 1;
    std::fs::write(&path, &raw).unwrap();
    assert!(EncryptedFileSecretStore::open(&path, &key).await.is_err());

    // Garbage / unknown format is refused (and not overwritten).
    std::fs::write(&path, b"{\"k\":\"v\"}").unwrap();
    assert!(EncryptedFileSecretStore::open(&path, &key).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"k\":\"v\"}");
}

#[cfg(unix)]
#[tokio::test]
async fn world_readable_files_are_restricted_on_open() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir();
    let (path, key_path) = (dir.join("secrets.enc"), dir.join("secrets.key"));
    let key = SecretStoreKey::load_or_create(&key_path).await.unwrap();
    EncryptedFileSecretStore::open(&path, &key)
        .await
        .unwrap()
        .store("k", secret("v"))
        .await
        .unwrap();
    for p in [&path, &key_path] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let key = SecretStoreKey::load_or_create(&key_path).await.unwrap();
    EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&key_path), 0o600);
}

#[tokio::test]
#[allow(deprecated)]
async fn legacy_obfuscated_store_is_imported_and_deleted() {
    let dir = temp_dir();
    let legacy = dir.join("secrets.bin");
    {
        let old = universal_ai::FileSecretStore::open(&legacy).await.unwrap();
        old.store("key_a", secret(API_KEY)).await.unwrap();
        old.store("key_b", secret("b-secret")).await.unwrap();
    }
    let path = dir.join("secrets.enc");
    let key = SecretStoreKey::generate().unwrap();
    let store = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    store.store("key_b", secret("already-here")).await.unwrap();

    assert_eq!(store.import_legacy_file(&legacy).await.unwrap(), 1);
    assert!(!legacy.exists(), "legacy file deleted");
    assert_eq!(
        store.get("key_a").await.unwrap().unwrap().expose_secret(),
        API_KEY
    );
    assert_eq!(
        store.get("key_b").await.unwrap().unwrap().expose_secret(),
        "already-here",
        "existing secrets win"
    );
    let reopened = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    assert!(reopened.get("key_a").await.unwrap().is_some(), "persisted");
    assert!(!contains(
        &std::fs::read(&path).unwrap(),
        API_KEY.as_bytes()
    ));
    // Nothing to import twice.
    assert_eq!(store.import_legacy_file(&legacy).await.unwrap(), 0);

    // An unreadable legacy file is an error and stays where it is.
    std::fs::write(&legacy, b"\x00\x01garbage").unwrap();
    assert!(store.import_legacy_file(&legacy).await.is_err());
    assert!(legacy.exists());
}

#[tokio::test]
async fn concurrent_writes_are_all_persisted() {
    let dir = temp_dir();
    let path = dir.join("secrets.enc");
    let key = SecretStoreKey::generate().unwrap();
    let store = Arc::new(EncryptedFileSecretStore::open(&path, &key).await.unwrap());
    let tasks: Vec<_> = (0..32)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.store(&format!("k{i}"), secret("v")).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let reopened = EncryptedFileSecretStore::open(&path, &key).await.unwrap();
    for i in 0..32 {
        assert!(
            reopened.get(&format!("k{i}")).await.unwrap().is_some(),
            "k{i}"
        );
    }
}

#[test]
fn key_format_is_validated_without_echoing_the_input() {
    assert!(SecretStoreKey::from_hex(&"ab".repeat(32)).is_ok());
    for bad in ["", "abcd", &"zz".repeat(32), &"ab".repeat(33)] {
        let err = SecretStoreKey::from_hex(bad).unwrap_err().to_string();
        assert!(bad.len() < 8 || !err.contains(bad), "{err}");
    }
}
