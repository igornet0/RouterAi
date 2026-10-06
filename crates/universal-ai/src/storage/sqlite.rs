//! SQLite persistence via sqlx.

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;

use crate::account::{Account, AccountStatus};
use crate::balance::Balance;
use crate::cost::Cost;
use crate::error::{AiError, AiResult};
use crate::storage::Storage;
use crate::types::{AccountId, Currency, KeyId, ModelId, ProviderId, RequestId};
use crate::usage::{validate_importance, RequestUsage, Usage};

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
        .map_err(|e| AiError::Storage {
            message: e.to_string(),
        })?;

        // Existing DBs created before content columns.
        for sql in [
            "ALTER TABLE requests ADD COLUMN request_json TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE requests ADD COLUMN response_json TEXT",
            "ALTER TABLE requests ADD COLUMN importance INTEGER",
        ] {
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                let msg = e.to_string();
                if !msg.to_ascii_lowercase().contains("duplicate column") {
                    return Err(AiError::Storage { message: msg });
                }
            }
        }
        Ok(())
    }

    fn map_request_row(row: &sqlx::sqlite::SqliteRow) -> RequestUsage {
        let request_id = RequestId::from_str(row.get::<String, _>("request_id").as_str())
            .unwrap_or_default();
        let cost_amount: Option<String> = row.try_get("cost_amount").ok();
        let request_json_raw: String = row
            .try_get("request_json")
            .unwrap_or_else(|_| "{}".into());
        let response_json_raw: Option<String> = row.try_get("response_json").ok().flatten();
        let importance: Option<i64> = row.try_get("importance").ok().flatten();
        RequestUsage {
            request_id,
            provider: ProviderId::new(row.get::<String, _>("provider")),
            account: AccountId::new(row.get::<String, _>("account")),
            api_key: row
                .try_get::<Option<String>, _>("api_key")
                .ok()
                .flatten()
                .map(KeyId::new),
            model: ModelId::new(row.get::<String, _>("model")),
            started_at: chrono::DateTime::parse_from_rfc3339(row.get::<String, _>("started_at").as_str())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            finished_at: chrono::DateTime::parse_from_rfc3339(
                row.get::<String, _>("finished_at").as_str(),
            )
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
            usage: Usage {
                prompt_tokens: row.get::<i64, _>("prompt_tokens") as u64,
                completion_tokens: row.get::<i64, _>("completion_tokens") as u64,
                total_tokens: row.get::<i64, _>("total_tokens") as u64,
                cached_tokens: None,
                reasoning_tokens: None,
            },
            cost: cost_amount.and_then(|s| {
                s.parse::<Decimal>().ok().map(|amount| Cost {
                    currency: Currency::usd(),
                    amount,
                    input_cost: Decimal::ZERO,
                    output_cost: Decimal::ZERO,
                    cache_cost: Decimal::ZERO,
                })
            }),
            success: row.get::<i64, _>("success") != 0,
            latency_ms: row.get::<i64, _>("latency_ms") as u64,
            request_json: serde_json::from_str(&request_json_raw)
                .unwrap_or_else(|_| serde_json::json!({})),
            response_json: response_json_raw.and_then(|s| serde_json::from_str(&s).ok()),
            importance: importance.map(|v| v as u8),
        }
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        let request_json = serde_json::to_string(&row.request_json).map_err(|e| {
            AiError::Serialization {
                message: e.to_string(),
            }
        })?;
        let response_json = row
            .response_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AiError::Serialization {
                message: e.to_string(),
            })?;
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO requests
            (request_id, provider, account, api_key, model, started_at, finished_at,
             prompt_tokens, completion_tokens, total_tokens, cost_amount, success, latency_ms,
             request_json, response_json, importance)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .execute(&self.pool)
        .await
        .map_err(|e| AiError::Storage {
            message: e.to_string(),
        })?;
        Ok(())
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
        let importance = validate_importance(importance).map_err(|message| {
            AiError::InvalidRequest { message }
        })?;
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
