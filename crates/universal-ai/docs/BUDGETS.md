# Budgets

## When a request is budget-controlled

A request is **controlled** when any limit applies: `BudgetPolicy::max_request_cost`,
`max_daily_cost`, `max_monthly_cost`, or per request `ChatBuilder::max_cost` /
`budget_scope(scope, Some(daily_limit))` — or when the caller asks for it without a
limit with `ChatBuilder::require_cost_bound()` (RouterAi agents always do).

Controlled requests are **fail-closed**: an attempt is dispatched only if its
worst case can be computed and fits every applicable limit.

| Condition | Error (nothing sent) |
|---|---|
| model has no price / no input or output rate | `PricingUnavailable` (`ErrorKind::Pricing`) |
| no `max_tokens` and no registered `max_output_tokens` | `OutputLimitUnknown` (`ErrorKind::Pricing`) |
| earlier attempts' charges + worst case > `max_request_cost` / `max_cost` | `BudgetExceeded` |
| committed today + worst case > daily limit (global or scope) | `DailyLimitExceeded` |
| committed this month + worst case > monthly limit | `MonthlyLimitExceeded` |
| an earlier controlled attempt on this provider / model was billed beyond its token bounds | `WorstCaseUnbounded` (`ErrorKind::Pricing`) |

### When the provider bills beyond the worst case

The worst case rests on two assumptions: billed input tokens ≤ the input bound
(UTF-8 bytes + overhead) and billed output tokens (reasoning included) ≤
`max_tokens`. If a settled, budget-controlled attempt reports more, the **actual
cost is charged** (never capped at the reservation — the ledger may then exceed a
limit by that difference), the overrun is logged at `error`, and the
provider / model pair is marked: every further budget-controlled request to it is
rejected before HTTP with `WorstCaseUnbounded` until the process restarts.
Attempts already in flight when the overrun is detected are not affected.
Uncontrolled requests are not blocked.

Periods are UTC calendar days / months. *Committed* = settled charges + open
reservations, recomputed from storage after a restart.

## Estimate, reservation, settlement

These are three different amounts on every attempt row (`CostAccounting`):

| Field | Meaning |
|---|---|
| `estimated_cost` | worst case computed before dispatch (see [PRICING](PRICING.md)) |
| `reserved_cost` | amount held in the ledger while the attempt is in flight (= estimate; `None` when uncontrolled) |
| `charged_cost` | what counts against budgets after settlement |
| `cost` (row) | actual usage × pricing, when determinable |

```text
reserve (atomic check + charge, Pending row persisted) → dispatch → settle exactly once
```

`SpendLedger::reserve` checks all limits and adds the worst case to the in-memory
totals under one lock, after persisting the `Pending` row. Concurrent requests
therefore cannot pass on the same headroom. Settlement persists the final row and
moves the totals from the reservation to the final charge; the difference is
*released*.

When the logical request uses the registry's `max_output_tokens` as output
bound, that value is written into the request as `max_tokens` so the provider
cannot exceed what was reserved.

## Unknown cost is never zero

| Outcome | Status | Charged (controlled) | Charged (uncontrolled) |
|---|---|---|---|
| usage + all needed rates known | `Actual` | actual cost | actual cost |
| response without usage | `UsageUnavailable` | reservation | estimate (if computable) |
| usage not priceable (missing rate, unknown category, inconsistent counts) | `PricingUnavailable` | reservation | estimate (if computable) |
| definitive provider rejection (4xx, 429, 500, 503) | `NotCharged` | 0 | 0 |
| timeout, transport error, malformed response, 502/504/524 | `UsageUnavailable` | reservation | estimate |
| cancelled / dropped after dispatch, no final usage | `Abandoned` | reservation | estimate |
| cancelled / dropped after final usage | `Abandoned` | actual cost | actual cost |
| cancelled before dispatch | `Abandoned` | 0 | 0 |
| rejected by the gate | `Rejected` | 0 (never sent) | — |
| corrected later (`AiClient::reconcile_attempt`) | `Reconciled` | reconciled amount | reconciled amount |

Uncontrolled requests with no price at all have no estimate: their charge stays
`None` and they are counted in `UsageStatistics::unknown_cost_requests`.
`MissingUsagePolicy::Reject` additionally fails a controlled request whose
response had no usage (`UsageUnavailable`), after charging the reservation.

## Pending requests, restart, crashes

* The `Pending` row (charged at the reservation) is written **before** anything
  is sent. A crash at any later point leaves either that row or the settled row.
* After a restart the ledger sums rows from storage: an orphaned `Pending` row
  keeps counting at its reservation — a possibly billed request never becomes free.
* `AiClient::recover_orphaned_reservations(process_start)` marks rows still
  `Pending` from before `process_start` as `Abandoned`; the charge is unchanged.
* A request future dropped in-process (e.g. a caller timeout) is settled as
  `Abandoned` by a task spawned from `Drop`; `ChatBuilder::cancel_on` settles
  synchronously before returning `AiError::Cancelled`.
* Settlement itself runs on its own task: dropping the caller while the final
  row is being persisted (ledger lock, slow SQLite write) does not interrupt it
  (`tests/settlement_cancellation.rs`).

Proven by `tests/crash_recovery.rs` (real SIGKILL after reserve, after provider
success before settlement, after settlement, with concurrent reservations, and
in WAL mode) and `tests/budget.rs::h_spend_survives_restart`.

## Fallback and retry accounting

Each physical attempt is its own row (`accounting.attempt`, `accounting.retry`,
shared `accounting.logical_request_id`). `AiClient::load_logical_request_attempts(id)`
returns all of them from storage (any attempt id works; also after a restart and
from another process); `logical_request_attempts(id)` answers from this process's
recent rows only. Sum `budget_charge()` for what a logical request cost
(`ChatBuilder::request_id` lets the caller know the id up front, also for failed
requests). RouterAi computes run cost from storage. See
[RETRY_AND_FALLBACK](RETRY_AND_FALLBACK.md).

## Reconciliation (refunds and corrections)

An attempt settled at a conservative charge (timeout, no usage, unpriceable
usage, abandoned stream) — or one later found billed (`NotCharged`) or billed
differently — can be corrected to what the provider reported:

```rust,ignore
client.reconcile_attempt(&attempt_id, Reconciliation::Usage { usage, source: "usage export".into() }).await?;
client.reconcile_attempt(&attempt_id, Reconciliation::Amount { amount, source: "invoice INV-1".into() }).await?;
```

* The row becomes `Reconciled`, `charged_cost` = the reconciled amount (usage is
  priced with the registry; unpriceable usage is `PricingUnavailable`), and
  `cost_note` keeps the source, previous status / charge and the adjustment.
* The ledger moves by the difference in the same storage write (SQLite: same
  transaction as the totals) — a refund releases budget, a surcharge consumes it.
* Same amount + same source again → no change (idempotent); a different amount
  corrects again.
* Refused: `Pending` attempts (in flight, or orphaned — recover them first),
  `Rejected` ones (never sent), negative amounts, empty sources.

Proven by `tests/accounting.rs`.

## Inspecting spend

```rust,ignore
client.budget_status(None).await?;                  // global day / month
client.budget_status(Some("agent:lineage")).await?; // scope
client.stats().all().await.charged_cost;            // this process's attempts only
```

`UsageStatistics` is in memory and incremental (O(1) per attempt): it equals the
ledger's sum only for spend made by this process (the ledger also counts rows
persisted by earlier runs and other processes).

## One budget per database (SQLite)

`SqliteStorage` reserves atomically (`Storage::atomic_reservations`): the limit
check and the `Pending` row are one `BEGIN IMMEDIATE` transaction against running
period totals (`spend_totals`) that every row write updates in the same
transaction. Every process and every `AiClient` using the database therefore
shares one budget — two reservations can never pass on the same headroom
(`tests/shared_budget.rs`: two clients, and three separate processes, admit
exactly what fits). A row write moves the totals by the difference to the
stored row, so replays change nothing and a crash leaves both or neither.

Other storages (`MemoryStorage`, custom `Storage` implementations that do not
override `reserve`) are decided by the client's in-process ledger: one
`AiClient` per storage. Wrappers around `SqliteStorage` that do not forward
`atomic_reservations` / `reserve` fall back to that mode too.

Upgrading: stop every process running an older version before starting the new
one — an older binary refuses a v3 database when it opens it, but one that is
already running would write rows without updating the totals.
