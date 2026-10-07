# Architecture

## Request lifecycle

```text
ChatBuilder::send / ::stream                       (public API — the only entry point)
  │ validate_content         non-text parts → UnsupportedCapability (no adapter sends them)
  ▼
AiClient::execute            logical request (logical_request_id, tracing span `ai.request`)
  │ candidates()             routing order; capability / model-limit / health filter
  │ for provider in candidates:                     (fallback loop)
  │   bind_provider()        select API key → adapter bound to that key
  │   loop:                                          (retry loop, same provider)
  ▼
AiClient::execute_attempt    one PHYSICAL attempt (span `ai.attempt`)
  │ preflight()              worst-case estimate → caps → SpendLedger::reserve
  │                          (Pending row persisted BEFORE dispatch) — or Rejected row
  │ AttemptMeter::new        owns the attempt's money from here on
  │ dispatched()             provider.chat / provider.stream_chat (deadline + cancel)
  ▼
Response / Stream            metered_stream wraps streams (pull-based)
  ▼
Usage (typed classes) → Cost (per class) or PricingGap
  ▼
Settlement                   AttemptMeter::settle_* / Drop  → exactly once
  ▼
Core::finish                 SpendLedger::settle (persist final row, move totals)
                             → key statistics → tracing event → telemetry → UsageManager
```

`execute_attempt` is the only code that calls `Provider::chat` /
`Provider::stream_chat`. Retries and fallbacks re-enter it, so every HTTP request
sent to a provider has its own reservation, row and settlement.

## Modules

| Module | Responsibility |
|---|---|
| `client` | public API, routing order, retry / fallback loop, budget gate |
| `execution` | attempt lifecycle (`AttemptMeter`), settlement rules, metered streams |
| `budget` | input / output bounds, worst case, `SpendLedger` (reservations, totals) |
| `cost` | `Cost`, `CostEstimate` (+ `explain()`), `PricingGap` |
| `pricing` | `ModelPricing`, `TokenClass`, rate policy, registry |
| `usage` | `Usage` (typed classes), `RequestUsage` rows, `CostAccounting`, statistics |
| `providers/*` | transport + protocol + usage extraction per provider |
| `http` | shared reqwest client, typed transport / status errors, `Retry-After` |
| `error` | `AiError`, `ErrorKind`, retry / fallback / financial classification |
| `storage` | `Storage` trait, memory and SQLite backends, migrations |
| `telemetry` | `TelemetrySink`, `AttemptReport` |
| `router` | routing *decisions* (`Router::select`) — never sends requests |

Provider-specific quirks (Anthropic cache classes, Gemini thinking tokens, OpenAI
usage details, SSE formats) live inside the adapters; the core sees only
canonical `Usage` and `StreamEvent`s.

## Paths that can reach a provider

| Path | Model request? | Accounting |
|---|---|---|
| `ChatBuilder::send` / `stream` | yes | full (single path) |
| `Router::select` | no (decision only; `Router::execute` was removed) | — |
| `AiClient::discover_models`, `check_provider_health`, `HealthMonitor` | no (`/models`, local checks) | — |
| `AiClient::balance_for`, `probe_balances`, `BalanceMonitor` | no (balance / cost endpoints) | — |
| `CostApi::estimate` / `worst_case` | no | — |
| Calling `Provider::chat` on an adapter **you hold** (e.g. `AiClient::providers()`, deprecated) | yes | **none** — escape hatch, documented |

## Known limits

* Adapter objects are ordinary values: code that holds one can call `chat` directly
  and bypass accounting. `AiClient::providers()` is deprecated for that reason;
  use `provider_summaries()`.
* The worst case assumes 1 input token ≤ 1 UTF-8 byte and that providers count
  reasoning tokens inside `max_tokens`. Prompt-cache writes are never requested; a
  response reporting them without a cache-write rate settles as
  `PricingUnavailable` at the reservation (not proven to cover the true cost).
* Non-text content (images, audio, files) is rejected — no adapter maps it.
* `HttpConfig::request_timeout` (default 60 s) also bounds whole streams.
* No local rate limiter / concurrency limiter: provider 429 / quota errors are
  typed (`ErrorKind::RateLimit`, `ErrorKind::ProviderQuota`) and kept separate from
  budgets.
* `UsageManager` keeps only recent attempt rows (10 000 by default,
  `AiClientBuilder::recent_attempts`, without content) and incremental
  statistics; `logical_request_attempts` / `request_usage` answer from those
  rows — `load_logical_request_attempts` / `get_ai_request` read storage.
* Reconciling an attempt that is no longer among the recent rows moves the
  ledger and storage but not this process's `stats()`; per-key statistics
  (`ApiKeyInfo::usage`, used by `LeastUsed` / `LowestCost` selection) are not
  adjusted by reconciliation.
* Cross-process budgets need `SqliteStorage` (atomic reservations); with
  `MemoryStorage` or a custom storage the budget is per `AiClient`.
* Tiered prices are keyed on input tokens only (see
  [PRICING](PRICING.md#tiered-long-context-prices)); `Marginal` is RouterAi's
  definition and must be checked against the provider's billing.
* A provider that bills beyond the reserved token bounds is charged its actual
  cost; the model is then refused for budget-controlled requests
  (`WorstCaseUnbounded`), but attempts already in flight can overrun as well.
* Streaming tool calls are not assembled (out of scope; agents use the
  non-streaming tool loop).
