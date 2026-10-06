# Security

## Secrets

- API keys are stored as `secrecy::SecretString`.
- `Debug` / `Display` for providers, builders, and `AddKeyRequest` **redact** secrets.
- `SecretStore` backends: `MemorySecretStore`, `FileSecretStore` (obfuscated at-rest, prefer OS keychain in prod), `KeychainSecretStore` (macOS-oriented; falls back safely in CI).
- Never log `expose_secret()` results. Tracing uses `key_id` / `provider` only.

## Errors & sanitization

- `AiError::sanitize_message` strips common key prefixes (`sk-`, `Bearer `, `AIza`, …).
- Provider error bodies are truncated; keys must not be interpolated into messages.

## Transport

- Shared `reqwest` client (rustls); certificate validation enabled.
- Configurable connect / request timeouts and max response size.
- HTTPS preferred for remote providers; HTTP allowed for local OpenAI-compatible servers (Ollama, etc.).

## Telemetry

- Disabled by default.
- Opt-in `TelemetrySink` receives ids / usage / latency only — **never** prompt or completion text.

## Tests

See unit tests in `secrets`, `error`, and integration `secrets_do_not_appear_in_debug_of_providers`.
