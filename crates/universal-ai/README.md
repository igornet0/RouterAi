# universal-ai

Provider-agnostic AI client runtime with **fail-closed cost control**: one
execution path for every model request, per-attempt financial accounting,
typed pricing, crash-safe budgets, metered streaming, and machine-readable errors.

```rust,ignore
use rust_decimal::Decimal;
use universal_ai::{AiClient, BudgetPolicy, ModelPricing, OpenAI, ProviderId};

let client = AiClient::builder()
    .provider(OpenAI::new("sk-...")?)
    .budget(BudgetPolicy::daily_usd(Decimal::new(5, 0)))
    .build()?;
client.pricing().upsert(ModelPricing::per_million(
    ProviderId::openai(), "gpt-4o-mini", Decimal::new(15, 2), Decimal::new(60, 2),
));
let reply = client.chat().model("gpt-4o-mini").message("Hello").max_tokens(200).send().await?;
println!("{} — {:?}", reply.text(), reply.cost());
```

Compiled versions of this and other examples are the crate-level doctests
(`src/lib.rs`).

## Guarantees (what the tests prove)

| Guarantee | Where it is enforced | Proven by |
|---|---|---|
| Every model request goes through one path | `AiClient::execute` → `execute_attempt` | `financial_invariants.rs` (`assert_invariants`: one row per dispatched call) |
| Unknown cost is never $0 | settlement (`execution.rs`) | `financial_invariants.rs`, `provider_usage.rs`, `execution` unit tests |
| With a budget, unknown / unbounded cost is rejected before HTTP | `preflight` | `rejected_requests_make_no_http_attempt`, `budget.rs` A/C |
| Each physical attempt (retry, fallback) has its own reservation + settlement | `execute_attempt` | `retries_are_separate_attempts_with_own_reservation` |
| Budget exhaustion stops further attempts | `preflight` per attempt | `budget_exhaustion_stops_further_attempts` |
| Concurrent requests cannot oversubscribe the budget | `SpendLedger::reserve` (one lock) | `concurrency.rs` (10/50/100/500 + randomized) |
| Spend survives restart and SIGKILL | reservation persisted before dispatch | `crash_recovery.rs` (real SIGKILL) |
| Streams settle exactly once, also when dropped | `metered_stream` + `AttemptMeter::drop` | `stream_*`, `dropped_stream_*` |
| Secrets never in errors / Debug / storage / stats / tracing / telemetry | adapters + `redact_secret` + `sanitize_message` | `secrets_regression.rs` |

What is **not** guaranteed is listed in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#known-limits).

## Documentation

| Topic | |
|---|---|
| [ARCHITECTURE](docs/ARCHITECTURE.md) | request lifecycle, modules, known limits |
| [CONFIGURATION](docs/CONFIGURATION.md) | `AiConfig`, `HttpConfig`, builder, per-request options |
| [PROVIDERS](docs/PROVIDERS.md) | adapters, capabilities, usage mapping, adding a provider |
| [PRICING](docs/PRICING.md) | token classes, rate policy, estimates |
| [BUDGETS](docs/BUDGETS.md) | reservation, settlement, unknown cost, restart, pending |
| [STREAMING](docs/STREAMING.md) | stream state machine, cancellation |
| [RETRY_AND_FALLBACK](docs/RETRY_AND_FALLBACK.md) | policy table, fallback accounting |
| [ERRORS](docs/ERRORS.md) | `ErrorKind`, classification |
| [OBSERVABILITY](docs/OBSERVABILITY.md) | tracing spans / events, telemetry |
| [SECURITY](docs/SECURITY.md) | secrets handling, content storage |
| [STORAGE](docs/STORAGE.md) | SQLite / memory backends, performance |
| [MIGRATIONS](docs/MIGRATIONS.md) | schema versions |
| [TESTING](docs/TESTING.md) | test suites, benchmarks |
