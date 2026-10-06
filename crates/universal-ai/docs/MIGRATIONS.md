# Migrations

`SqliteStorage::connect` migrates the database in place and records the schema
version in `PRAGMA user_version` (`universal_ai::storage::SQLITE_SCHEMA_VERSION`).

| Version | Changes |
|---|---|
| 0 (no `user_version`) | initial `requests` / `balances` / `accounts` tables |
| 1 (implicit, column-detected) | `request_json`, `response_json`, `importance`; accounting: `cost_status`, `estimated_cost`, `charged_cost`, `budget_scope`, `logical_request_id`, `attempt`, `rejection` |
| 2 | `reserved_cost`, `retry`, `dispatched`, `cost_note`, `error_kind`, `cached_tokens`, `cache_creation_tokens`, `reasoning_tokens`, `other_tokens`, `cost_json`; index on `cost_status`; `user_version = 2` |

Rules:

* Migrations are additive and idempotent (`ADD COLUMN`, duplicate columns
  ignored), so re-opening is safe and old files open without manual steps.
* Old rows keep `NULL` in new columns: a v0 row with only `cost_amount` still
  counts that amount; a v1 `Pending` row still counts its `charged_cost`
  (`migrates_v0_*`, `migrates_v1_*` tests). Nothing becomes free.
* A database with a **newer** `user_version` is refused (`ErrorKind::Storage`):
  an older binary must not reinterpret — and possibly undercount — newer rows.
* `enable_wal()` changes the journal mode persistently; it is not a schema
  change and older binaries can open WAL databases.
