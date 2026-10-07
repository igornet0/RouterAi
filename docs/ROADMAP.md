# RouterAi roadmap to production

RouterAi is **not** production-ready because the unit/integration gate is
green. It is ready once it has been proven embedded in real applications —
only then is the public API frozen and a release candidate cut.

```
FOUNDATION                          INTEGRATION
  P2.5 Accounting        ✅
  P2.2 CI + MSRV         🔄
  P2.3 Tiered pricing
  Provider policy
  Minimal trace + concurrency cap
  Provider matrix  ───────────────▶  Integration Lab (apps A–D)
  Rate limiting                      Real API runs
  Retry / circuit breaker            Fault injection, kill -9 / restart
  Period billing reconciliation      Load + soak
              └──────────┬──────────┘
                 Security + reliability audit
                         │
                 Pilot (shadow → 5 → 25 → 50 → 100 %)
                         │
                 API adjustments → API freeze → RC
                         │
                 Rollout: simple → medium → complex project
```

## Invariants

Every gate below is checked against these:

- **No free money** — no request is sent without a reservation that covers it.
- **No double charge**, **no double refund**.
- **No budget bypass** — including agent loops, retries, fallbacks, streaming.
- **No lost accounting** — across crashes, restarts, cancellations.
- **No secret leak** — logs, `Debug`/`Display`, panics, errors, metrics,
  traces, SQLite, HTTP responses.
- Prompt/content never reaches telemetry by default.

## Backlog (in order)

### 1. P2.2 — CI + MSRV
Push through a PR, fix only real CI failures. Do not keep expanding CI.
**Done when** CI (fmt, clippy, tests on Linux/macOS, MSRV 1.88, web build) is green.

### 2. P2.3 — Tiered pricing
- Pricing modes, explicit per model: `WholeRequest` (the whole request is
  billed at the tier its size falls into) vs `Marginal` (first N tokens at
  tier A, the rest at tier B). No implicit assumptions.
- Per tier: input threshold, input, cached input, cache write, output prices,
  application mode.
- Reservation is worst case: if a request may cross into a more expensive
  tier, the reservation uses the expensive tier.
- A request without `max_tokens` has no output upper bound → explicit rule
  (default cap or refusal), otherwise it is a budget bypass.
- History: price sheets are immutable versions in their own table; each cost
  record stores `pricing_version` (id/hash), not a copy of the sheet, so
  SQLite growth stays bounded and price changes never rewrite history.
- SQLite migration: old records readable, new records written, restart safe.
- Tests: below / exact / above threshold, both modes, input / output / cached
  input, reserve / settle / refund, restart, reconciliation.

### 3. Provider policy
Extend the existing `Router` (alongside `KeySelectionStrategy`, `MaxCost`) —
no second router. Policy answers "may this request be sent, and where?":
provider / model / key allowed, budget available, estimated cost acceptable,
provider healthy, rate limit available, fallback allowed.
Policy **decides**; execution stays separate.

### 4. Minimal observability + concurrency cap (before the Lab)
- Per attempt: `request_id`, `logical_request_id`, `attempt_id`, provider,
  key (id only), model, policy decision, estimated / reserved / actual cost,
  latency, tokens, retry count, error. Enough to answer "why did this request
  go here and what did it really cost?".
- Local concurrency cap so the long-running Lab app cannot cause a 429 storm.
- Minimal secret-leak check before any real key is used in the Lab.

### 5. Provider verification matrix
Existing adapters (Anthropic, Gemini, OpenAI, OpenAI-compatible, DeepSeek,
OpenRouter) — verify, do not rewrite.

| Scenario | Real API | Fault server |
|---|---|---|
| completion, streaming, tool calling | ✓ | ✓ |
| usage, cost, token accounting | ✓ | ✓ |
| 401/403 | ✓ | ✓ |
| missing / wrong usage, malformed response | | ✓ |
| 4xx, 429, 5xx | | ✓ |
| timeout, disconnect, cancellation, retry | | ✓ |

The fault server (controllable proxy) is built once and reused for fault
injection (step 10).

Real-API runs are **not** required PR checks: separate scheduled/manual
workflow with secrets, its spend capped by RouterAi's own budget.

### 6. Integration Lab ⭐
Test RouterAi as a library, from the consumer side.

```
integration/
├── openai/ anthropic/ gemini/ openrouter/
├── streaming/ tools/ accounting/ failures/ budgets/
└── applications/
```

- **App A — minimal CLI:** app → RouterAi → OpenAI. Init, config, keys,
  request, response, errors, logging, shutdown, restart. Then add Anthropic
  and check fallback.
- **App B — streaming + tools:** partial chunks, disconnect, cancel, tool
  call → tool result → next model call. Every request of an agent loop must
  land in accounting.
- **App C — long-running service:** hours/days of real traffic. Memory, CPU,
  latency, SQLite growth, `recent_attempts`, `stats()`, locks, concurrency,
  rate limits, provider failures. `kill -9` during reserve / request /
  streaming / settlement / reconciliation → correct state after restart.
- **App D — long-running agent:** many keys and models, strict budget.

Every API pain point found here (builder, provider config, error types,
streaming, tools, policy, budget, observability) is logged for step 14.

### 7. Rate limiter
Design driven by Lab findings: local concurrency, RPM, TPM; per provider,
key, model. Single process first — no distributed limiter yet, but the
design must allow one later.

### 8. Retry + circuit breaker
Finish `retry.rs` / `health.rs`. Retry only 429, 500, 502, 503, timeout,
connection reset — never 400, 401, 403, invalid request, budget or policy
rejection. Retries strictly bounded.
Breaker: healthy → failures → open → cooldown → half-open → healthy.
Verified inside a Lab app.

### 9. Period billing reconciliation
RouterAi attempts → aggregate per period → compare with the provider's
usage/cost report. A delta belongs to the period, never to one attempt.
`BillingReconciliation`: provider, account/project, period, routerai_total,
provider_total, delta, currency, source, timestamp, status.

### 10. Fault injection
Provider unavailable, DNS failure, timeout, 429, 500, connection reset,
process kill, disk full, `SQLITE_BUSY`, corrupted response, missing / wrong
usage — each checked against the invariants.

### 11. Load + soak
1 / 10 / 100 / 500 / 1000 concurrent requests (where realistic): throughput,
p50/p95/p99, memory, CPU, SQLite latency, lock contention. Soak for hours/days:
leaks, unbounded structures, deadlocks, SQLite growth, accounting drift.

### 12. Security audit
Secrets in every output channel (see invariants), encrypted secret store,
server authentication / authorization, pricing and admin endpoints,
configuration.

### 13. Pilot
One real, non-business-critical project: shadow → 5 % → 25 % → 50 % → 100 %.
At each stage compare error rate, latency, cost, provider distribution, retry
rate, budget usage, accounting delta, application-level success.

### 14. API adjustments
Fix what the Lab and the pilot found — before 1.0.

### 15. API freeze

### 16. Release candidate
Gate: CI, MSRV, Linux, macOS, providers, pricing, accounting, reconciliation,
rate limits, retry, circuit breaker, security, restart, crash recovery, load,
soak, fault injection, real applications.

### 17. Rollout
Not everywhere at once:
- **Project A (simple):** single process, single provider, plain completion.
- **Project B (medium):** several providers, streaming, tools, budgets, fallback.
- **Project C (complex):** long-running agent, high concurrency, many keys and
  models, strict budget.

RouterAi is ready for the remaining projects once it has passed all three.
