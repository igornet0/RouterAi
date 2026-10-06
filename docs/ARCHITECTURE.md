# Architecture — universal_ai

## Goal

`universal_ai` is an infrastructure AI runtime layer (not a thin OpenAI SDK wrapper).
Applications talk to a unified client; providers are adapters behind a capability system.
Accounts, keys, balance, usage, cost, routing, and limits are first-class.

## Layering

```
Application / BoardDo / SaaS
        │
        ▼
   AiClient (public API)
        │
  ┌─────┼──────────────────────────────┐
  │ Chat / Stream / Models / Router    │
  │ Accounts / Keys / Balance / Cost   │
  │ Usage / Stats / Health / Events    │
  └─────┼──────────────────────────────┘
        ▼
   Provider trait + CapabilityRegistry
        │
  ┌─────┼──────────┬──────────┐
  ▼     ▼          ▼          ▼
OpenAI DeepSeek Anthropic  OpenAI-compatible …
        │
        ▼
   HttpClient (reqwest, shared)
```

## Design rules

1. **OpenAI-compatible is an adapter**, not the internal model.
2. **Capabilities gate optional APIs** — unsupported ops return `UnsupportedCapability`.
3. **Canonical types** live in core; adapters map to/from provider wire formats.
4. **Pricing is external** — never hardcode live tariffs inside provider adapters.
5. **Secrets never appear** in `Debug`/`Display`/logs/errors/telemetry.
6. **Escape hatches** (`extensions` / `raw`) preserve provider-specific features.
7. **Fallback is side-effect safe** — no auto-retry of ambiguous mutating calls.

## Crate layout (Phase 1+)

Workspace starts as:

| Crate | Role |
|-------|------|
| `universal-ai` | Library: core + providers + account/cost/storage modules |
| `universal-ai-cli` | `ai` CLI binary |

Modules inside `universal-ai` mirror future crate splits so extraction stays cheap.

```
src/
  lib.rs
  error.rs
  types/          # ids, messages, chat, usage, cost, balance, health…
  capability.rs
  provider.rs     # Provider trait
  http.rs
  client/         # AiClient, builders, chat fluent API
  config.rs
  retry.rs
  rate_limit.rs
  events.rs
  telemetry.rs
  secrets/
  account/
  pricing/
  cost/
  usage/
  balance/
  models/
  router/
  storage/
  providers/      # openai, deepseek, anthropic, gemini, openrouter, compatible
```

## Phases

See README / implementation changelog. Phase 1 delivers end-to-end OpenAI-compatible chat + streaming.

## Public entry points

- `AiClient::builder()`
- `client.chat()…send()` / `.stream()`
- `client.models()`, `client.accounts()`, `client.cost()`, `client.stats()`, `client.router()`, `client.monitor()`, `client.events()`
- `Provider` + `Capability` for extensions
