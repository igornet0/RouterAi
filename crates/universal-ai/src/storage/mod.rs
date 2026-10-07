//! Persistence abstraction — core does not hard-depend on SQLite.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::sync::RwLock;

use crate::account::Account;
use crate::balance::Balance;
use crate::error::{AiError, AiResult};
use crate::types::RequestId;
use crate::usage::{validate_importance, CostStatus, RequestUsage};

/// Spend limits a reservation must fit (see [`Storage::reserve`]). `None` = no
/// limit. Periods are UTC calendar days / months.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SpendLimits {
    /// Global daily limit.
    pub daily: Option<Decimal>,
    /// Global monthly limit.
    pub monthly: Option<Decimal>,
    /// Daily limit of the row's budget scope (only when it has one).
    pub scope_daily: Option<Decimal>,
}

/// Storage backend for history / accounts.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Persist request usage row.
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()>;
    /// Persist usage alias.
    async fn save_usage(&self, row: &RequestUsage) -> AiResult<()> {
        self.save_request(row).await
    }
    /// Load a single request by id.
    async fn get_request(&self, id: &RequestId) -> AiResult<Option<RequestUsage>>;
    /// Set optional importance rating (0–10) for a stored request.
    async fn set_importance(
        &self,
        id: &RequestId,
        importance: Option<u8>,
    ) -> AiResult<RequestUsage>;
    /// Persist balance snapshot.
    async fn save_balance(&self, balance: &Balance) -> AiResult<()>;
    /// Persist account.
    async fn save_account(&self, account: &Account) -> AiResult<()>;
    /// Load recent requests.
    async fn list_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>>;
    /// Load balances.
    async fn list_balances(&self) -> AiResult<Vec<Balance>>;
    /// Load accounts.
    async fn list_accounts(&self) -> AiResult<Vec<Account>>;

    /// Budget spend of rows started in `[start, end)` (UTC), optionally only rows
    /// tagged with `scope`. Sums [`RequestUsage::budget_charge`]. The default scans
    /// [`Storage::list_requests`]; persistent backends should override it.
    async fn spend_in_window(
        &self,
        scope: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> AiResult<Decimal> {
        Ok(self
            .list_requests(usize::MAX)
            .await?
            .iter()
            .filter(|r| r.started_at >= start && r.started_at < end)
            .filter(|r| scope.is_none() || r.accounting.budget_scope.as_deref() == scope)
            .map(RequestUsage::budget_charge)
            .sum())
    }

    /// Every persisted attempt of the logical request `logical` (the first
    /// attempt's id), in attempt order. The default scans
    /// [`Storage::list_requests`]; persistent backends should override it.
    async fn list_attempts(&self, logical: &RequestId) -> AiResult<Vec<RequestUsage>> {
        let mut rows: Vec<RequestUsage> = self
            .list_requests(usize::MAX)
            .await?
            .into_iter()
            .filter(|r| r.accounting.logical_request_id.unwrap_or(r.request_id) == *logical)
            .collect();
        rows.sort_by_key(|r| r.accounting.attempt);
        Ok(rows)
    }

    /// Whether [`Storage::reserve`] decides atomically for every process sharing
    /// this storage. When `false` (the default) the client's in-process ledger
    /// decides — correct only while a single client uses the storage.
    fn atomic_reservations(&self) -> bool {
        false
    }

    /// Atomically check `limits` against the committed spend (settled + reserved)
    /// of `row`'s periods and persist `row` (a `Pending` reservation): two
    /// reservations can never both be admitted on the same headroom. Called only
    /// when [`Storage::atomic_reservations`] is `true`.
    async fn reserve(&self, row: &RequestUsage, limits: &SpendLimits) -> AiResult<()> {
        let _ = (row, limits);
        Err(AiError::Storage {
            message: "atomic reservations are not supported by this storage".into(),
        })
    }

    /// Mark rows still [`CostStatus::Pending`] that started before `before` as
    /// [`CostStatus::Abandoned`], keeping their charge. Returns the count.
    async fn abandon_pending(&self, before: DateTime<Utc>) -> AiResult<u64> {
        let mut n = 0;
        for mut row in self.list_requests(usize::MAX).await? {
            if row.accounting.status == CostStatus::Pending && row.started_at < before {
                row.accounting.status = CostStatus::Abandoned;
                row.accounting.cost_note =
                    Some("orphaned reservation recovered after restart; charge kept".into());
                self.save_request(&row).await?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// In-memory storage.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    requests: RwLock<Vec<RequestUsage>>,
    balances: RwLock<Vec<Balance>>,
    accounts: RwLock<Vec<Account>>,
}

impl MemoryStorage {
    /// Create.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn save_request(&self, row: &RequestUsage) -> AiResult<()> {
        let mut g = self.requests.write().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        if let Some(existing) = g.iter_mut().find(|r| r.request_id == row.request_id) {
            *existing = row.clone();
        } else {
            g.push(row.clone());
        }
        Ok(())
    }

    async fn get_request(&self, id: &RequestId) -> AiResult<Option<RequestUsage>> {
        let g = self.requests.read().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        Ok(g.iter().find(|r| &r.request_id == id).cloned())
    }

    async fn set_importance(
        &self,
        id: &RequestId,
        importance: Option<u8>,
    ) -> AiResult<RequestUsage> {
        let importance = validate_importance(importance)
            .map_err(|message| AiError::InvalidRequest { message })?;
        let mut g = self.requests.write().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        let row = g
            .iter_mut()
            .find(|r| &r.request_id == id)
            .ok_or_else(|| AiError::NotFound {
                message: format!("request {id}"),
            })?;
        row.importance = importance;
        Ok(row.clone())
    }

    async fn save_balance(&self, balance: &Balance) -> AiResult<()> {
        let mut g = self.balances.write().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        if let Some(existing) = g.iter_mut().find(|b| b.provider == balance.provider) {
            *existing = balance.clone();
        } else {
            g.push(balance.clone());
        }
        Ok(())
    }

    async fn save_account(&self, account: &Account) -> AiResult<()> {
        let mut g = self.accounts.write().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        if let Some(existing) = g.iter_mut().find(|a| a.id == account.id) {
            *existing = account.clone();
        } else {
            g.push(account.clone());
        }
        Ok(())
    }

    async fn list_requests(&self, limit: usize) -> AiResult<Vec<RequestUsage>> {
        let g = self.requests.read().map_err(|_| AiError::Storage {
            message: "lock poisoned".into(),
        })?;
        Ok(g.iter().rev().take(limit).cloned().collect())
    }

    async fn list_balances(&self) -> AiResult<Vec<Balance>> {
        Ok(self
            .balances
            .read()
            .map_err(|_| AiError::Storage {
                message: "lock poisoned".into(),
            })?
            .clone())
    }

    async fn list_accounts(&self) -> AiResult<Vec<Account>> {
        Ok(self
            .accounts
            .read()
            .map_err(|_| AiError::Storage {
                message: "lock poisoned".into(),
            })?
            .clone())
    }
}

#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteStorage, SCHEMA_VERSION as SQLITE_SCHEMA_VERSION};

/// Helper timestamp for migrations / tests.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}
