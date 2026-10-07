//! SQLite persistence via sqlx.

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::sqlite::{SqliteConnection, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Utc};

use crate::account::{Account, AccountStatus};
use crate::balance::Balance;
use crate::cost::Cost;
use crate::error::{sanitize_message, AiError, AiResult};
use crate::storage::{SpendLimits, Storage};
use crate::types::{AccountId, Currency, KeyId, ModelId, ProviderId, RequestId};
use crate::usage::{validate_importance, CostAccounting, RequestUsage, Usage};

/// SQLite-backed storage.
#[derive(Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    /// Open (creates file + schema).
    pub async fn connect(url: &str) -> AiResult<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;
        let s = Self { pool };
        s.migrate().await?;
        Ok(s)
    }

    async fn migrate(&self) -> AiResult<()> {
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await
            .map_err(storage_err)?;
        if version > SCHEMA_VERSION {
            // Fail closed: an older binary must not reinterpret (and possibly
            // undercount) rows written by a newer schema.
            return Err(AiError::Storage {
                message: format!(
                    "database schema version {version} is newer than supported {SCHEMA_VERSION}"
                ),
            });
        }
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_checked_at TEXT
            );
            CREATE TABLE IF NOT EXISTS balances (
                provider TEXT PRIMARY KEY,
                currency TEXT NOT NULL,
                total TEXT NOT NULL,
                available TEXT,
                granted TEXT,
                topped_up TEXT,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS requests (
                request_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                account TEXT NOT NULL,
                api_key TEXT,
                model TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                cost_amount TEXT,
                success INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                request_json TEXT NOT NULL DEFAULT '{}',
                response_json TEXT,
                importance INTEGER
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        // Additive, idempotent column migrations (v0 → v2). Old rows keep NULLs,
        // which read back as "unknown" — never as a zero charge.
        for sql in [
            // v1: content columns.
            "ALTER TABLE requests ADD COLUMN request_json TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE requests ADD COLUMN response_json TEXT",
            "ALTER TABLE requests ADD COLUMN importance INTEGER",
            // v1: budget accounting (one row per physical attempt).
            "ALTER TABLE requests ADD COLUMN cost_status TEXT",
            "ALTER TABLE requests ADD COLUMN estimated_cost TEXT",
            "ALTER TABLE requests ADD COLUMN charged_cost TEXT",
            "ALTER TABLE requests ADD COLUMN budget_scope TEXT",
            "ALTER TABLE requests ADD COLUMN logical_request_id TEXT",
            "ALTER TABLE requests ADD COLUMN attempt INTEGER",
            "ALTER TABLE requests ADD COLUMN rejection TEXT",
            // v2: per-attempt reservation, typed usage, cost breakdown.
            "ALTER TABLE requests ADD COLUMN reserved_cost TEXT",
            "ALTER TABLE requests ADD COLUMN retry INTEGER",
            "ALTER TABLE requests ADD COLUMN dispatched INTEGER",
            "ALTER TABLE requests ADD COLUMN cost_note TEXT",
            "ALTER TABLE requests ADD COLUMN error_kind TEXT",
            "ALTER TABLE requests ADD COLUMN cached_tokens INTEGER",
            "ALTER TABLE requests ADD COLUMN cache_creation_tokens INTEGER",
            "ALTER TABLE requests ADD COLUMN reasoning_tokens INTEGER",
            "ALTER TABLE requests ADD COLUMN other_tokens TEXT",
            "ALTER TABLE requests ADD COLUMN cost_json TEXT",
        ] {
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                let msg = e.to_string();
                if !msg.to_ascii_lowercase().contains("duplicate column") {
                    return Err(AiError::Storage { message: msg });
                }
            }
        }
        for sql in [
            "CREATE INDEX IF NOT EXISTS idx_requests_started_at ON requests(started_at)",
            "CREATE INDEX IF NOT EXISTS idx_requests_cost_status ON requests(cost_status)",
            "CREATE INDEX IF NOT EXISTS idx_requests_logical ON requests(logical_request_id)",
        ] {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        // v3: running spend totals, maintained in the same transaction as every
        // row write. Built once from the rows, under the write lock, so concurrent
        // openers cannot build them twice.
        let mut tx = self.begin_immediate().await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_err)?;
        if version > SCHEMA_VERSION {
            return Err(AiError::Storage {
                message: format!(
                    "database schema version {version} is newer than supported {SCHEMA_VERSION}"
                ),
            });
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS spend_totals (bucket TEXT PRIMARY KEY, amount TEXT NOT NULL)",
        )
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
        if version < 3 {
            rebuild_totals(&mut tx).await?;
        }
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&mut *tx)
            .await
            .map_err(storage_err)?;
        tx.commit().await.map_err(storage_err)
    }

    /// Write transaction that takes SQLite's write lock up front: reads inside it
    /// see the state the write is based on (no lost update between processes).
    async fn begin_immediate(&self) -> AiResult<sqlx::Transaction<'static, sqlx::Sqlite>> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_err)
    }

    /// Switch the database to write-ahead logging (persistent for the file).
    /// Commits stay durable (`synchronous` remains FULL) but need one fsync
    /// instead of several: ~4–5× lower reservation / settlement latency in
    /// `benches/overhead.rs`. Not for databases on network filesystems.
    pub async fn enable_wal(&self) -> AiResult<()> {
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode=WAL")
            .fetch_one(&self.pool)
            .await
            .map_err(storage_err)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(AiError::Storage {
                message: format!("could not enable WAL (journal_mode={mode})"),
            });
        }
        Ok(())
    }

    /// Schema version recorded in the database (`PRAGMA user_version`).
    pub async fn schema_version(&self) -> AiResult<i64> {
        sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await
            .map_err(storage_err)
    }

    fn map_request_row(row: &sqlx::sqlite::SqliteRow) -> RequestUsage {
        let request_id =
            RequestId::from_str(row.get::<String, _>("request_id").as_str()).unwrap_or_default();
        let text = |col: &str| -> Option<String> { row.try_get(col).ok().flatten() };
        let int = |col: &str| -> Option<i64> { row.try_get(col).ok().flatten() };
        let decimal = |col: &str| text(col).and_then(|s| s.parse::<Decimal>().ok());
        let accounting = CostAccounting {
            status: text("cost_status")
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
                .unwrap_or_default(),
            estimated_cost: decimal("estimated_cost"),
            reserved_cost: decimal("reserved_cost"),
            charged_cost: decimal("charged_cost"),
            budget_scope: text("budget_scope"),
            logical_request_id: text("logical_request_id")
                .and_then(|s| RequestId::from_str(&s).ok()),
            attempt: int("attempt").unwrap_or(0) as u32,
            retry: int("retry").unwrap_or(0) as u32,
            dispatched: int("dispatched").unwrap_or(0) != 0,
            rejection: text("rejection"),
            cost_note: text("cost_note"),
            error_kind: text("error_kind")
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok()),
        };
        // Full breakdown when present; legacy rows only stored the total.
        let cost = text("cost_json")
            .and_then(|s| serde_json::from_str::<Cost>(&s).ok())
            .or_else(|| {
                decimal("cost_amount").map(|amount| Cost {
                    amount,
                    ..Cost::zero()
                })
            });
        let parse_time = |col: &str| {
            chrono::DateTime::parse_from_rfc3339(row.get::<String, _>(col).as_str())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now())
        };
        RequestUsage {
            request_id,
            provider: ProviderId::new(row.get::<String, _>("provider")),
            account: AccountId::new(row.get::<String, _>("account")),
            api_key: text("api_key").map(KeyId::new),
            model: ModelId::new(row.get::<String, _>("model")),
            started_at: parse_time("started_at"),
            finished_at: parse_time("finished_at"),
            usage: Usage {
                prompt_tokens: row.get::<i64, _>("prompt_tokens") as u64,
                completion_tokens: row.get::<i64, _>("completion_tokens") as u64,
                total_tokens: row.get::<i64, _>("total_tokens") as u64,
                cached_tokens: int("cached_tokens").map(|v| v as u64),
                cache_creation_tokens: int("cache_creation_tokens").map(|v| v as u64),
                reasoning_tokens: int("reasoning_tokens").map(|v| v as u64),
                other_tokens: text("other_tokens")
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
            },
            cost,
            success: row.get::<i64, _>("success") != 0,
            latency_ms: row.get::<i64, _>("latency_ms") as u64,
            request_json: text("request_json")
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| serde_json::json!({})),
            response_json: text("response_json").and_then(|s| serde_json::from_str(&s).ok()),
            importance: int("importance").map(|v| v as u8),
            accounting,
        }
    }
}

/// Current schema version (`PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 3;

fn storage_err(e: sqlx::Error) -> AiError {
    AiError::Storage {
        message: sanitize_message(&e.to_string()),
    }
}

fn enum_text<T: serde::Serialize>(v: &T) -> Option<String> {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

/// `INSERT OR REPLACE` of one attempt row (no totals bookkeeping).
async fn upsert_row(conn: &mut SqliteConnection, row: &RequestUsage) -> AiResult<()> {
    let request_json =
        serde_json::to_string(&row.request_json).map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;
    let response_json = row
        .response_json
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;
    let other_tokens = (!row.usage.other_tokens.is_empty())
        .then(|| serde_json::to_string(&row.usage.other_tokens))
        .transpose()
        .map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;
    let cost_json = row
        .cost
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| AiError::Serialization {
            message: e.to_string(),
        })?;
    let a = &row.accounting;
    sqlx::query(
        r#"
        INSERT OR REPLACE INTO requests
        (request_id, provider, account, api_key, model, started_at, finished_at,
         prompt_tokens, completion_tokens, total_tokens, cost_amount, success, latency_ms,
         request_json, response_json, importance,
         cost_status, estimated_cost, charged_cost, budget_scope, logical_request_id,
         attempt, rejection,
         reserved_cost, retry, dispatched, cost_note, error_kind,
         cached_tokens, cache_creation_tokens, reasoning_tokens, other_tokens, cost_json)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(row.request_id.to_string())
    .bind(row.provider.to_string())
    .bind(row.account.to_string())
    .bind(row.api_key.as_ref().map(|k| k.to_string()))
    .bind(row.model.to_string())
    .bind(row.started_at.to_rfc3339())
    .bind(row.finished_at.to_rfc3339())
    .bind(row.usage.prompt_tokens as i64)
    .bind(row.usage.completion_tokens as i64)
    .bind(row.usage.total_tokens as i64)
    .bind(row.cost.as_ref().map(|c| c.amount.to_string()))
    .bind(row.success as i64)
    .bind(row.latency_ms as i64)
    .bind(request_json)
    .bind(response_json)
    .bind(row.importance.map(|v| v as i64))
    .bind(enum_text(&a.status))
    .bind(a.estimated_cost.map(|d| d.to_string()))
    .bind(a.charged_cost.map(|d| d.to_string()))
    .bind(a.budget_scope.clone())
    .bind(a.logical_request_id.map(|id| id.to_string()))
    .bind(a.attempt as i64)
    .bind(a.rejection.clone())
    .bind(a.reserved_cost.map(|d| d.to_string()))
    .bind(a.retry as i64)
    .bind(a.dispatched as i64)
    .bind(a.cost_note.clone())
    .bind(a.error_kind.as_ref().and_then(enum_text))
    .bind(row.usage.cached_tokens.map(|v| v as i64))
    .bind(row.usage.cache_creation_tokens.map(|v| v as i64))
    .bind(row.usage.reasoning_tokens.map(|v| v as i64))
    .bind(other_tokens)
    .bind(cost_json)
    .execute(&mut *conn)
    .await
    .map_err(storage_err)?;
    Ok(())
}

/// Budget bucket key: scope (`g` = global, `s:<name>`) and period (`D<yyyy-mm-dd>`
/// or `M<yyyy-mm>`, UTC). The period's fixed shape at the end keeps keys unique.
fn bucket(scope: Option<&str>, period: &str) -> String {
    match scope {
        None => format!("g|{period}"),
        Some(s) => format!("s:{s}|{period}"),
    }
}

fn day_period(at: DateTime<Utc>) -> String {
    format!("D{}", at.format("%Y-%m-%d"))
}

fn month_period(at: DateTime<Utc>) -> String {
    format!("M{}", at.format("%Y-%m"))
}

/// Buckets a row's charge counts toward.
fn row_buckets(scope: Option<&str>, at: DateTime<Utc>) -> Vec<String> {
    let (day, month) = (day_period(at), month_period(at));
    let mut out = vec![bucket(None, &day), bucket(None, &month)];
    if let Some(s) = scope {
        out.push(bucket(Some(s), &day));
        out.push(bucket(Some(s), &month));
    }
    out
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// What a stored row counts against budgets ([`RequestUsage::budget_charge`]).
fn stored_charge(charged: Option<String>, cost_amount: Option<String>) -> Decimal {
    charged
        .and_then(|s| s.parse().ok())
        .or_else(|| cost_amount.and_then(|s| s.parse().ok()))
        .unwrap_or(Decimal::ZERO)
}

async fn bucket_total(conn: &mut SqliteConnection, bucket: &str) -> AiResult<Decimal> {
    let amount: Option<String> =
        sqlx::query_scalar("SELECT amount FROM spend_totals WHERE bucket = ?")
            .bind(bucket)
            .fetch_optional(&mut *conn)
            .await
            .map_err(storage_err)?;
    amount
        .map(|a| {
            a.parse().map_err(|_| AiError::Storage {
                message: format!("corrupt spend total for {bucket}"),
            })
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Persist `row` and move every affected spend total by the difference between
/// its new charge and the charge stored before (none for a new row). Replaying
/// a write changes nothing; a crash leaves both or neither (one transaction).
async fn write_row(conn: &mut SqliteConnection, row: &RequestUsage) -> AiResult<()> {
    let old = sqlx::query(
        "SELECT started_at, budget_scope, charged_cost, cost_amount FROM requests \
         WHERE request_id = ?",
    )
    .bind(row.request_id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage_err)?;
    upsert_row(conn, row).await?;

    let mut deltas: HashMap<String, Decimal> = HashMap::new();
    let new_charge = row.budget_charge();
    for b in row_buckets(row.accounting.budget_scope.as_deref(), row.started_at) {
        *deltas.entry(b).or_default() += new_charge;
    }
    if let Some(old) = old {
        let charge = stored_charge(
            old.try_get("charged_cost").ok().flatten(),
            old.try_get("cost_amount").ok().flatten(),
        );
        // An old row with an unreadable timestamp was never counted.
        if let Some(at) = parse_time(&old.get::<String, _>("started_at")) {
            let scope: Option<String> = old.try_get("budget_scope").ok().flatten();
            for b in row_buckets(scope.as_deref(), at) {
                *deltas.entry(b).or_default() -= charge;
            }
        }
    }
    for (b, delta) in deltas {
        if delta.is_zero() {
            continue;
        }
        let total = bucket_total(conn, &b).await? + delta;
        sqlx::query(
            "INSERT INTO spend_totals (bucket, amount) VALUES (?, ?) \
             ON CONFLICT(bucket) DO UPDATE SET amount = excluded.amount",
        )
        .bind(&b)
        .bind(total.to_string())
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

/// Recompute every spend total from the rows (migration to schema v3).
async fn rebuild_totals(conn: &mut SqliteConnection) -> AiResult<()> {
    sqlx::query("DELETE FROM spend_totals")
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    let rows =
        sqlx::query("SELECT started_at, budget_scope, charged_cost, cost_amount FROM requests")
            .fetch_all(&mut *conn)
            .await
            .map_err(storage_err)?;
    let mut totals: HashMap<String, Decimal> = HashMap::new();
    for r in rows {
        let Some(at) = parse_time(&r.get::<String, _>("started_at")) else {
            continue;
        };
        let scope: Option<String> = r.try_get("budget_scope").ok().flatten();
        let charge = stored_charge(
            r.try_get("charged_cost").ok().flatten(),
            r.try_get("cost_amount").ok().flatten(),
        );
        for b in row_buckets(scope.as_deref(), at) {
            *totals.entry(b).or_default() += charge;
        }
    }
    for (b, amount) in totals {
        sqlx::query("INSERT INTO spend_totals (bucket, amount) VALUES (?, ?)")
            .bind(b)
            .bind(amount.to_string())
            .execute(&mut *conn)
            .await
            .map_err(storage_err)?;
    }
    Ok(())
}

/// The bucket for exactly one UTC day or month window, if `[start, end)` is one.
fn window_bucket(scope: Option<&str>, start: DateTime<Utc>, end: DateTime<Utc>) -> Option<String> {
    let midnight = start.time() == chrono::NaiveTime::MIN;
    if !midnight {
        return None;
    }
    if end == start + chrono::Duration::days(1) {
        return Some(bucket(scope, &day_period(start)));
    }
    let d = start.date_naive();
    let next_month = if d.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
    }?;
    (d.day() == 1 && end.date_naive() == next_month && end.time() == chrono::NaiveTime::MIN)
        .then(|| bucket(scope, &month_period(start)))
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        let mut tx = self.begin_immediate().await?;
        write_row(&mut tx, row).await?;
        tx.commit().await.map_err(storage_err)
    }

    fn atomic_reservations(&self) -> bool {
        true
    }

    async fn list_attempts(&self, logical: &RequestId) -> AiResult<Vec<RequestUsage>> {
        let id = logical.to_string();
        let rows = sqlx::query(
            "SELECT * FROM requests WHERE logical_request_id = ? \
             OR (logical_request_id IS NULL AND request_id = ?)",
        )
        .bind(&id)
        .bind(&id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut rows: Vec<RequestUsage> = rows.iter().map(Self::map_request_row).collect();
        rows.sort_by_key(|r| r.accounting.attempt);
        Ok(rows)
    }

    async fn reserve(&self, row: &RequestUsage, limits: &SpendLimits) -> AiResult<()> {
        let mut tx = self.begin_immediate().await?;
        let scope = row.accounting.budget_scope.as_deref();
        let (day, month) = (day_period(row.started_at), month_period(row.started_at));
        let day_spent = bucket_total(&mut tx, &bucket(None, &day)).await?;
        let month_spent = bucket_total(&mut tx, &bucket(None, &month)).await?;
        let scope_day = match scope {
            Some(s) => Some(bucket_total(&mut tx, &bucket(Some(s), &day)).await?),
            None => None,
        };
        crate::budget::check_limits(
            row.budget_charge(),
            scope,
            limits,
            day_spent,
            month_spent,
            scope_day,
        )?;
        write_row(&mut tx, row).await?;
        tx.commit().await.map_err(storage_err)
    }

    async fn get_request(&self, id: &RequestId) -> AiResult<Option<RequestUsage>> {
        let row = sqlx::query("SELECT * FROM requests WHERE request_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;
        Ok(row.map(|r| Self::map_request_row(&r)))
    }

    async fn set_importance(
        &self,
        id: &RequestId,
        importance: Option<u8>,
    ) -> AiResult<RequestUsage> {
        let importance = validate_importance(importance)
            .map_err(|message| AiError::InvalidRequest { message })?;
        let result = sqlx::query("UPDATE requests SET importance = ? WHERE request_id = ?")
            .bind(importance.map(|v| v as i64))
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;
        if result.rows_affected() == 0 {
            return Err(AiError::NotFound {
                message: format!("request {id}"),
            });
        }
        self.get_request(id)
            .await?
            .ok_or_else(|| AiError::NotFound {
                message: format!("request {id}"),
            })
    }

    async fn save_balance(&self, balance: &Balance) -> AiResult<()> {
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO balances
            (provider, currency, total, available, granted, topped_up, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(balance.provider.to_string())
        .bind(balance.currency.to_string())
        .bind(balance.total.to_string())
        .bind(balance.available.map(|d| d.to_string()))
        .bind(balance.granted.map(|d| d.to_string()))
        .bind(balance.topped_up.map(|d| d.to_string()))
        .bind(balance.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| AiError::Storage {
            message: e.to_string(),
        })?;
        Ok(())
    }

    async fn save_account(&self, account: &Account) -> AiResult<()> {
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO accounts
            (id, provider, name, status, created_at, last_checked_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(account.id.to_string())
        .bind(account.provider.to_string())
        .bind(&account.name)
        .bind(format!("{:?}", account.status).to_ascii_lowercase())
        .bind(account.created_at.to_rfc3339())
        .bind(account.last_checked_at.map(|t| t.to_rfc3339()))
        .execute(&self.pool)
        .await
        .map_err(|e| AiError::Storage {
            message: e.to_string(),
        })?;
        Ok(())
    }

    async fn list_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>> {
        let rows = sqlx::query("SELECT * FROM requests ORDER BY started_at DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;

        Ok(rows.iter().map(Self::map_request_row).collect())
    }

    async fn spend_in_window(
        &self,
        scope: Option<&str>,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> AiResult<Decimal> {
        // Budget periods are kept as running totals, updated with every row.
        if let Some(b) = window_bucket(scope, start, end) {
            let mut conn = self.pool.acquire().await.map_err(storage_err)?;
            return bucket_total(&mut conn, &b).await;
        }
        // `started_at` is RFC 3339 text: prefilter by date prefix in SQL, then compare
        // parsed timestamps exactly; sum as Decimal (SQL SUM would go through floats).
        let rows = sqlx::query(
            "SELECT started_at, charged_cost, cost_amount, budget_scope FROM requests \
             WHERE started_at >= ? AND (? IS NULL OR budget_scope = ?)",
        )
        .bind(start.format("%Y-%m-%d").to_string())
        .bind(scope)
        .bind(scope)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AiError::Storage {
            message: e.to_string(),
        })?;
        let mut total = Decimal::ZERO;
        for row in rows {
            let Ok(at) =
                chrono::DateTime::parse_from_rfc3339(row.get::<String, _>("started_at").as_str())
            else {
                continue;
            };
            let at = at.with_timezone(&chrono::Utc);
            if at < start || at >= end {
                continue;
            }
            let amount = |col: &str| -> Option<Decimal> {
                row.try_get::<Option<String>, _>(col)
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
            };
            total += amount("charged_cost")
                .or_else(|| amount("cost_amount"))
                .unwrap_or(Decimal::ZERO);
        }
        Ok(total)
    }

    async fn abandon_pending(&self, before: chrono::DateTime<chrono::Utc>) -> AiResult<u64> {
        let rows = sqlx::query(
            "SELECT request_id, started_at FROM requests WHERE cost_status = 'pending'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut n = 0;
        for row in rows {
            let started =
                chrono::DateTime::parse_from_rfc3339(row.get::<String, _>("started_at").as_str())
                    .map(|d| d.with_timezone(&chrono::Utc));
            // Unparseable timestamps are left pending (still charged).
            let Ok(started) = started else { continue };
            if started >= before {
                continue;
            }
            n += sqlx::query(
                "UPDATE requests SET cost_status = 'abandoned', cost_note = ? \
                 WHERE request_id = ? AND cost_status = 'pending'",
            )
            .bind("orphaned reservation recovered after restart; charge kept")
            .bind(row.get::<String, _>("request_id"))
            .execute(&self.pool)
            .await
            .map_err(storage_err)?
            .rows_affected();
        }
        Ok(n)
    }

    async fn list_balances(&self) -> AiResult<Vec<Balance>> {
        let rows = sqlx::query("SELECT * FROM balances")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;
        let mut out = Vec::new();
        for row in rows {
            let parse_opt = |key: &str| -> Option<Decimal> {
                row.try_get::<Option<String>, _>(key)
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
            };
            out.push(Balance {
                provider: ProviderId::new(row.get::<String, _>("provider")),
                currency: Currency::new(row.get::<String, _>("currency")),
                total: row
                    .get::<String, _>("total")
                    .parse()
                    .unwrap_or(Decimal::ZERO),
                available: parse_opt("available"),
                granted: parse_opt("granted"),
                topped_up: parse_opt("topped_up"),
                updated_at: chrono::DateTime::parse_from_rfc3339(
                    row.get::<String, _>("updated_at").as_str(),
                )
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            });
        }
        Ok(out)
    }

    async fn list_accounts(&self) -> AiResult<Vec<Account>> {
        let rows = sqlx::query("SELECT * FROM accounts")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AiError::Storage {
                message: e.to_string(),
            })?;
        let mut out = Vec::new();
        for row in rows {
            let status = match row.get::<String, _>("status").as_str() {
                "disabled" => AccountStatus::Disabled,
                "unhealthy" => AccountStatus::Unhealthy,
                _ => AccountStatus::Active,
            };
            out.push(Account {
                id: AccountId::new(row.get::<String, _>("id")),
                provider: ProviderId::new(row.get::<String, _>("provider")),
                name: row.get("name"),
                status,
                credit_budget: None,
                created_at: chrono::DateTime::parse_from_rfc3339(
                    row.get::<String, _>("created_at").as_str(),
                )
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
                last_checked_at: row
                    .try_get::<Option<String>, _>("last_checked_at")
                    .ok()
                    .flatten()
                    .and_then(|s| {
                        chrono::DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|d| d.with_timezone(&chrono::Utc))
                    }),
            });
        }
        Ok(out)
    }
}
