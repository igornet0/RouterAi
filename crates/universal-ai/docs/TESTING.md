# Testing

```bash
cargo test -p universal-ai --all-features    # unit + integration + doctests
cargo bench -p universal-ai --bench overhead  # benchmarks (release)
```

No test needs network access or real keys: providers are mocked in-process
(`tests/common::Scripted`, deterministic per call index / request) or with
`wiremock` HTTP servers.

| Suite | Covers |
|---|---|
| unit tests (`src/**`) | pricing policy, token breakdown, cost per class, estimates, error classification, `Retry-After`, settlement rules (`execution`), sanitization |
| `financial_invariants.rs` | normal / retry / timeout / fallback / malformed / cancel / dropped future / streams (complete, no usage, duplicate usage, network error, dropped, dropped after usage) / cumulative caps / usage beyond the reserved bounds (`WorstCaseUnbounded`); every test checks the ledger invariants |
| `settlement_cancellation.rs` | the caller (request future / stream) dropped *while* its settlement is being persisted: the row still settles and reaches the statistics |
| `accounting.rs` | attempts loaded from storage after eviction and restart; reconciliation: refund, surcharge, idempotent replay, refusals (pending, rejected, negative, no source, unpriceable, unknown id), SQLite and memory, rows outside the recent window |
| `shared_budget.rs` | one budget per SQLite database: two clients and three separate processes admit exactly what fits; running totals equal the rows after mixed traffic (scopes, retries, timeouts, streams, drops); replayed settlement; v2 → v3 rebuild |
| `output_limits.rs` | every adapter sends the output bound in its reasoning-inclusive parameter (chat + stream), pins the registry bound, refuses unbounded budgeted requests before HTTP |
| `concurrency.rs` | 10 / 50 / 100 / 500 concurrent mixed requests (success, timeout, 503, no usage, stream) and randomized mixes; `committed ≤ budget` at every write and at every dispatch |
| `crash_recovery.rs` | SIGKILL of a child process after reserve, before settlement, after settlement (also WAL), with concurrent reservations; schema migrations v0 / v1, newer-schema refusal, column round trip |
| `provider_usage.rs` | OpenAI cached / reasoning / audio, Anthropic cache writes and SSE, Gemini thinking tokens and unexplained totals, malformed / null / duplicate SSE usage, validation before HTTP |
| `pricing_properties.rs` | property tests (deterministic xorshift): `cost ≥ 0`, `cost ≤ estimate` within bounds, inconsistent / uncategorized usage never priced, idempotent settlement writes |
| `secrets_regression.rs` | key echoed by the provider never appears in errors, Debug, rows, statistics, key metadata, telemetry, or TRACE logs |
| `budget.rs`, `key_binding.rs`, `tool_calling.rs`, `sqlite_storage.rs`, `integration_providers.rs` | pre-existing behaviour |

Invariants asserted by `tests/common::assert_invariants` after each scenario:

```text
sum(budget_charge(rows)) == budget_status().daily_spent == stats().charged_cost
status == Actual      ⇒ charged_cost == actual cost
status == Rejected    ⇒ not dispatched, charged 0
dispatched rows       == provider calls          (one row per physical attempt)
no row left Pending; charges and reservations ≥ 0
```

Property-style tests use a fixed-seed generator (no extra dependencies) so
failures are reproducible.
