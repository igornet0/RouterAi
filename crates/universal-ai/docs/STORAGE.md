# Storage

`Storage` persists one `RequestUsage` row per physical attempt plus balances and
accounts. Budget totals are **derived from rows** (`spend_in_window` sums
`RequestUsage::budget_charge`), so there is no second source of truth to drift.

| Backend | Use |
|---|---|
| `MemoryStorage` (default) | tests / ephemeral processes; spend is lost on exit; upserts are O(rows) |
| `SqliteStorage::connect("sqlite://path?mode=rwc")` | durable; recommended for anything with a budget |

## Durability

* Each write is one `BEGIN IMMEDIATE` SQLite transaction (default
  `synchronous = FULL`): committed rows survive process kills (`crash_recovery.rs`).
* The reservation row is committed **before** the request is dispatched, in the
  same transaction as the limit check; the settlement row replaces it
  (`INSERT OR REPLACE` by attempt id).
* `spend_totals(bucket, amount)` holds the committed spend per UTC day / month,
  globally and per budget scope, as exact decimal text. Every row write updates
  it by the difference to the previously stored row in the same transaction —
  replaying a write is idempotent, and totals always equal the rows
  (`shared_budget.rs::running_totals_equal_the_rows_after_mixed_traffic`).
  Budget-period reads are O(1); other windows scan the rows.
* If a settlement write fails, the error is logged and the reservation stays
  charged (in memory and in the persisted `Pending` row).
* Orphaned `Pending` rows: see [BUDGETS](BUDGETS.md#pending-requests-restart-crashes).

## Columns (schema v3)

`requests`: ids, provider, account, `api_key` (record id), model, timestamps,
token counts (`prompt_tokens`, `completion_tokens`, `total_tokens`,
`cached_tokens`, `cache_creation_tokens`, `reasoning_tokens`, `other_tokens` JSON),
`cost_amount` + `cost_json` (per-class breakdown), `success`, `latency_ms`,
`request_json` / `response_json` (unless content storage is disabled), `importance`,
accounting (`cost_status`, `estimated_cost`, `reserved_cost`, `charged_cost`,
`budget_scope`, `logical_request_id`, `attempt`, `retry`, `dispatched`,
`rejection`, `cost_note`, `error_kind`). Money is stored as decimal text and summed
as `Decimal` (never floats).

## Performance

Measured with `cargo bench -p universal-ai --bench overhead` (zero-latency
in-process provider; numbers are per request). Durable-commit numbers depend
almost entirely on the disk's fsync latency: two Apple Silicon machines differed
by ~3× on the SQLite rows.

| Measurement | Result |
|---|---|
| adapter call alone | 1.7 µs |
| full path, no budget, memory | ~15 µs |
| full path, budget (reserve + settle), memory | ~18 µs |
| full path, budget, SQLite (default journal) | 0.64–2.5 ms (two fsync'd commits: reservation + settlement) |
| full path, budget, SQLite WAL (`enable_wal()`) | 0.14–0.7 ms |
| budget-period spend (`spend_in_window` for a UTC day / month, SQLite `spend_totals`) | ~30 µs, independent of row count (a row scan over 2 000 rows took ~2 ms before schema v3) |
| metered stream, 102 events | ~26 µs (raw adapter 2.4 µs) |
| throughput, memory, 1 → 512 in flight | ~85k–100k req/s (no lock collapse) |
| throughput, SQLite, 1 → 512 in flight | ~0.8–1.5k req/s |
| throughput, SQLite WAL, 1 → 512 in flight | ~2.4–7k req/s |

Settlement runs on its own Tokio task (so a dropped caller cannot interrupt it);
that costs ~8 µs per request on the in-memory path and is invisible next to a
durable commit.

The global ledger lock is **not** the bottleneck: in memory it sustains ~90k
req/s at 512 in flight. With SQLite the ceiling is the single-writer durable
commit; the lock merely serializes what SQLite serializes anyway.
Options, in order of cost:

1. `SqliteStorage::enable_wal()` — ~3.5–4.6× faster commits, same durability
   (`synchronous` stays FULL). Not for network filesystems.
2. Group commit: batch reservations of concurrent requests into one transaction
   (keeps "persisted before dispatch"; adds a few ms of latency per batch).
3. Shard the ledger lock per budget scope (only helps once storage is faster).

Compared with provider latency (0.3–60 s per request) the default overhead is
below 0.3 %.
