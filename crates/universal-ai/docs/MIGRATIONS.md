# Migrations

`SqliteStorage::connect` migrates the database in place and records the schema
version in `PRAGMA user_version` (`universal_ai::storage::SQLITE_SCHEMA_VERSION`).

| Version | Changes |
|---|---|
| 0 (no `user_version`) | initial `requests` / `balances` / `accounts` tables |
| 1 (implicit, column-detected) | `request_json`, `response_json`, `importance`; accounting: `cost_status`, `estimated_cost`, `charged_cost`, `budget_scope`, `logical_request_id`, `attempt`, `rejection` |
| 2 | `reserved_cost`, `retry`, `dispatched`, `cost_note`, `error_kind`, `cached_tokens`, `cache_creation_tokens`, `reasoning_tokens`, `other_tokens`, `cost_json`; index on `cost_status`; `user_version = 2` |
| 3 | `spend_totals` (running committed spend per UTC day / month, global and per scope), rebuilt from the rows once, inside a `BEGIN IMMEDIATE` transaction that re-checks `user_version` (concurrent openers cannot build it twice); `user_version = 3` |

Rules:

* Migrations are additive and idempotent (`ADD COLUMN`, duplicate columns
  ignored), so re-opening is safe and old files open without manual steps.
* Old rows keep `NULL` in new columns: a v0 row with only `cost_amount` still
  counts that amount; a v1 `Pending` row still counts its `charged_cost`
  (`migrates_v0_*`, `migrates_v1_*` tests). Nothing becomes free.
* A database with a **newer** `user_version` is refused (`ErrorKind::Storage`):
  an older binary must not reinterpret — and possibly undercount — newer rows.
  A v2 binary opening a v3 database is refused for the same reason (it would
  write rows without updating `spend_totals`); stop old processes before
  upgrading (`schema_v2_database_gets_totals_rebuilt_from_its_rows`).
* `enable_wal()` changes the journal mode persistently; it is not a schema
  change and older binaries can open WAL databases.
