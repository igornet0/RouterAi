# Budgets

## When a request is budget-controlled

A request is **controlled** when any limit applies: `BudgetPolicy::max_request_cost`,
`max_daily_cost`, `max_monthly_cost`, or per request `ChatBuilder::max_cost` /
`budget_scope(scope, Some(daily_limit))`.

Controlled requests are **fail-closed**: an attempt is dispatched only if its
worst case can be computed and fits every applicable limit.

| Condition | Error (nothing sent) |
|---|---|
| model has no price / no input or output rate | `PricingUnavailable` (`ErrorKind::Pricing`) |
| no `max_tokens` and no registered `max_output_tokens` | `OutputLimitUnknown` (`ErrorKind::Pricing`) |
| earlier attempts' charges + worst case > `max_request_cost` / `max_cost` | `BudgetExceeded` |
| committed today + worst case > daily limit (global or scope) | `DailyLimitExceeded` |
| committed this month + worst case > monthly limit | `MonthlyLimitExceeded` |

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

Proven by `tests/crash_recovery.rs` (real SIGKILL after reserve, after provider
success before settlement, after settlement, with concurrent reservations, and
in WAL mode) and `tests/budget.rs::h_spend_survives_restart`.

## Fallback and retry accounting

Each physical attempt is its own row (`accounting.attempt`, `accounting.retry`,
shared `accounting.logical_request_id`). `AiClient::logical_request_attempts(id)`
returns all of them; sum `budget_charge()` for what a logical request cost
(`ChatBuilder::request_id` lets the caller know the id up front, also for failed
requests). See [RETRY_AND_FALLBACK](RETRY_AND_FALLBACK.md).

## Inspecting spend

```rust,ignore
client.budget_status(None).await?;                  // global day / month
client.budget_status(Some("agent:lineage")).await?; // scope
client.stats().all().await.charged_cost;            // equals the ledger's sum
```
