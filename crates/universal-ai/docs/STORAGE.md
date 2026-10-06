# Storage

`Storage` persists one `RequestUsage` row per physical attempt plus balances and
accounts. Budget totals are **derived from rows** (`spend_in_window` sums
`RequestUsage::budget_charge`), so there is no second source of truth to drift.

| Backend | Use |
|---|---|
| `MemoryStorage` (default) | tests / ephemeral processes; spend is lost on exit; upserts are O(rows) |
| `SqliteStorage::connect("sqlite://path?mode=rwc")` | durable; recommended for anything with a budget |

## Durability

* Each write is its own SQLite transaction (default `synchronous = FULL`):
  committed rows survive process kills (`crash_recovery.rs`).
* The reservation row is committed **before** the request is dispatched; the
  settlement row replaces it (`INSERT OR REPLACE` by attempt id — replaying a
  write is idempotent).
* If a settlement write fails, the error is logged and the reservation stays
  charged (in memory and in the persisted `Pending` row).
* Orphaned `Pending` rows: see [BUDGETS](BUDGETS.md#pending-requests-restart-crashes).

## Columns (schema v2)

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

Measured with `cargo bench -p universal-ai --bench overhead` (Apple Silicon
laptop, zero-latency in-process provider; numbers are per request):

| Measurement | Result |
|---|---|
| adapter call alone | 1.6 µs |
| full path, no budget, memory | ~10 µs |
| full path, budget (reserve + settle), memory | ~10 µs |
| full path, budget, SQLite (default journal) | ~640–800 µs (reservation ~300 µs + settlement ~330 µs: two fsync'd commits) |
| full path, budget, SQLite WAL (`enable_wal()`) | ~140 µs (reservation ~62 µs, settlement ~72 µs) |
| `spend_in_window` over 2 000 rows (cold ledger load, once per period / scope) | ~2.1 ms |
| metered stream, 102 events | 16 µs (raw adapter 2.3 µs) |
| throughput, memory, 1 → 512 in flight | ~130k → ~100k req/s (no lock collapse) |
| throughput, SQLite, 1 → 512 in flight | ~1.3–1.5k req/s (flat) |
| throughput, SQLite WAL, 1 → 512 in flight | ~7k req/s (flat) |

The global ledger lock is **not** the bottleneck: in memory it sustains ~100k
req/s at 512 in flight. With SQLite the ceiling is the single-writer durable
commit; the lock merely serializes what SQLite serializes anyway.
Options, in order of cost:

1. `SqliteStorage::enable_wal()` — ~4.6× faster commits, same durability
   (`synchronous` stays FULL). Not for network filesystems.
2. Group commit: batch reservations of concurrent requests into one transaction
   (keeps "persisted before dispatch"; adds a few ms of latency per batch).
3. Shard the ledger lock per budget scope (only helps once storage is faster).

Compared with provider latency (0.3–60 s per request) the default overhead is
below 0.3 %.
