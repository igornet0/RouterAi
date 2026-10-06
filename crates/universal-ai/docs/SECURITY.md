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

Managed keys live only in the `SecretStore`; each attempt binds the selected key
to a fresh adapter instance that is dropped afterwards
([ACCOUNT_MANAGEMENT](../../../docs/ACCOUNT_MANAGEMENT.md)).

Not covered here (out of scope of this library stage): `FileSecretStore` uses
XOR obfuscation, not encryption; `KeychainSecretStore` falls back to memory.

## Content

* Prompt / response content is never logged.
* By default the full request / response JSON is stored in request rows (used by
  the console). Set `AiConfig::store_request_content = false` to keep only
  metadata (`model`, message count, tool names, `max_tokens`).
* `CostEstimate::explain()` contains sizes and rates only.
