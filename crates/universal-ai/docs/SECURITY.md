# Security

## Secrets

| Surface | Mechanism | Test |
|---|---|---|
| adapters / credentials `Debug` | manual `Debug` with `<redacted>` (`OpenAICompatible`, `Anthropic`, `Gemini`, `DeepSeek`, `ProviderCredential`, `AddKeyRequest`) | `secrets_regression.rs` |
| errors | exact-value redaction of the sent credential (`redact_secret`) + `sanitize_message` for key-like patterns (every occurrence) on HTTP bodies, transport and storage errors | `secrets_regression.rs`, `error` unit tests |
| storage rows / statistics / key metadata | only key record ids (`KeyId`) are stored; `ApiKeyInfo` has no secret field | `secrets_regression.rs`, `budget.rs::n_*` |
| tracing | only ids, sizes, money; no headers, no content | `secrets_regression.rs` (captures TRACE output) |
| telemetry | `AttemptReport` (ids, numbers) | `secrets_regression.rs` |
| URLs | Gemini sends its key in `x-goog-api-key`, not the query string | — |
| files at rest | `EncryptedFileSecretStore` (see below) | `encrypted_secret_store.rs` |

Managed keys live only in the `SecretStore`; each attempt binds the selected key
to a fresh adapter instance that is dropped afterwards
([ACCOUNT_MANAGEMENT](../../../docs/ACCOUNT_MANAGEMENT.md)).

## Secret stores

| Store | At rest |
|---|---|
| `EncryptedFileSecretStore` | AES-256-GCM over the whole map (fresh random nonce per write, versioned header as AAD); file `0600`, replaced atomically; wider permissions are tightened on open. Wrong key, tampering or unknown format → `AiError::SecretStore`, never an empty store. `SecretStoreKey` comes from 64 hex chars or a `0600` key file (`load_or_create`) — keep it outside the data directory. |
| `FileSecretStore` (deprecated) | XOR obfuscation — **not** encryption. Migrate with `EncryptedFileSecretStore::import_legacy_file` (imports, persists, deletes the old file). |
| `KeychainSecretStore` | falls back to memory (no OS keychain integration yet). |
| `MemorySecretStore` | process memory only. |

Proven by `tests/encrypted_secret_store.rs` (no plaintext or single-byte-XOR copy
of the secret on disk, `0600` files, wrong key / flipped bit / garbage rejected,
legacy import, concurrent writes).

## Content

* Prompt / response content is never logged.
* By default the full request / response JSON is stored in request rows (used by
  the console). Set `AiConfig::store_request_content = false` to keep only
  metadata (`model`, message count, tool names, `max_tokens`).
* `CostEstimate::explain()` contains sizes and rates only.
