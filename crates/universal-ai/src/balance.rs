//! Balance types and monitoring.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

use crate::error::{AiError, AiResult};
use crate::provider::DynProvider;
use crate::types::{Currency, ProviderId};

/// How a balance figure was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceSource {
    /// Live provider balance endpoint (e.g. DeepSeek `/user/balance`).
    ProviderApi,
    /// OpenAI Organization Costs API (admin key) — spend, not prepaid remainder.
    CostsApi,
    /// Operator-entered credit budget minus locally tracked spend.
    ManualBudget,
    /// No figure available.
    None,
}

/// Status of a balance probe for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceProbeStatus {
    /// Live or estimated figure is available.
    Ok,
    /// Provider cannot expose balance with the current key type.
    Unsupported,
    /// Probe failed.
    Error,
    /// Estimated from manual budget.
    Estimated,
}

/// UI/API-friendly balance probe result (never invents provider prepaid balance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBalanceReport {
    /// Provider id.
    pub provider: ProviderId,
    /// Probe outcome.
    pub status: BalanceProbeStatus,
    /// Where numbers came from.
    pub source: BalanceSource,
    /// Currency when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<Currency>,
    /// Total / available from provider when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Decimal>,
    /// Available credits when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<Decimal>,
    /// Spend reported by provider costs API (window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_spend: Option<Decimal>,
    /// Locally tracked spend through this AiClient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked_spend: Option<Decimal>,
    /// Operator credit budget on the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_budget: Option<Decimal>,
    /// Account id when budget applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<crate::types::AccountId>,
    /// Human-readable explanation (no secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Observation time.
    pub updated_at: DateTime<Utc>,
}

/// Account balance snapshot from a provider. Never invent values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    /// Provider.
    pub provider: ProviderId,
    /// Currency.
    pub currency: Currency,
    /// Total balance when reported.
    pub total: Decimal,
    /// Available.
    pub available: Option<Decimal>,
    /// Granted credits.
    pub granted: Option<Decimal>,
    /// Topped up amount.
    pub topped_up: Option<Decimal>,
    /// When observed.
    pub updated_at: DateTime<Utc>,
}

/// Balance manager caches latest snapshots.
#[derive(Debug, Default)]
pub struct BalanceManager {
    latest: RwLock<Vec<Balance>>,
}

impl BalanceManager {
    /// Create empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Save snapshot.
    pub async fn save(&self, balance: Balance) {
        let mut g = self.latest.write().await;
        if let Some(existing) = g.iter_mut().find(|b| b.provider == balance.provider) {
            *existing = balance;
        } else {
            g.push(balance);
        }
    }

    /// List cached.
    pub async fn list(&self) -> Vec<Balance> {
        self.latest.read().await.clone()
    }

    /// Get one provider.
    pub async fn get(&self, provider: &ProviderId) -> Option<Balance> {
        self.latest
            .read()
            .await
            .iter()
            .find(|b| &b.provider == provider)
            .cloned()
    }
}

/// Monitor configuration.
#[derive(Debug, Clone)]
pub struct BalanceMonitorConfig {
    /// Poll interval.
    pub interval: Duration,
    /// Warning threshold on available/total.
    pub warning_threshold: Option<Decimal>,
    /// Critical threshold.
    pub critical_threshold: Option<Decimal>,
}

impl Default for BalanceMonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            warning_threshold: Some(Decimal::new(5, 0)),
            critical_threshold: Some(Decimal::new(1, 0)),
        }
    }
}

/// Monitor events.
#[derive(Debug, Clone)]
pub enum BalanceEvent {
    /// Fresh balance.
    Updated(Balance),
    /// Below warning threshold.
    LowBalance(Balance),
    /// Below critical threshold.
    CriticalBalance(Balance),
    /// Probe failed (safe message).
    CheckFailed {
        /// Provider.
        provider: ProviderId,
        /// Error display.
        message: String,
    },
}

/// Background balance poller with broadcast stream.
pub struct BalanceMonitor {
    providers: Vec<DynProvider>,
    manager: Arc<BalanceManager>,
    config: BalanceMonitorConfig,
    tx: broadcast::Sender<BalanceEvent>,
    running: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl BalanceMonitor {
    /// Create monitor.
    pub fn new(
        providers: Vec<DynProvider>,
        manager: Arc<BalanceManager>,
        config: BalanceMonitorConfig,
    ) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            providers,
            manager,
            config,
            tx,
            running: RwLock::new(None),
        }
    }

    /// Configure interval (builder-style).
    pub fn every(mut self, interval: Duration) -> Self {
        self.config.interval = interval;
        self
    }

    /// Subscribe to events (`while let Ok(ev) = rx.recv().await`).
    pub fn subscribe(&self) -> broadcast::Receiver<BalanceEvent> {
        self.tx.subscribe()
    }

    /// Alias used in public API examples.
    pub fn next_receiver(&self) -> broadcast::Receiver<BalanceEvent> {
        self.subscribe()
    }

    /// Start polling.
    pub async fn start(self: &Arc<Self>) -> AiResult<()> {
        let mut g = self.running.write().await;
        if g.is_some() {
            return Ok(());
        }
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            loop {
                this.poll_once().await;
                tokio::time::sleep(this.config.interval).await;
            }
        });
        *g = Some(handle);
        Ok(())
    }

    /// Stop polling.
    pub async fn stop(&self) -> AiResult<()> {
        if let Some(handle) = self.running.write().await.take() {
            handle.abort();
        }
        Ok(())
    }

    /// Run a single poll (also used by tests).
    pub async fn poll_once(&self) {
        for provider in &self.providers {
            if !provider.supports(crate::capability::Capability::Balance) {
                continue;
            }
            match provider.balance().await {
                Ok(Some(balance)) => {
                    self.manager.save(balance.clone()).await;
                    let _ = self.tx.send(BalanceEvent::Updated(balance.clone()));
                    let value = balance.available.unwrap_or(balance.total);
                    if let Some(crit) = self.config.critical_threshold {
                        if value <= crit {
                            let _ = self.tx.send(BalanceEvent::CriticalBalance(balance));
                            continue;
                        }
                    }
                    if let Some(warn) = self.config.warning_threshold {
                        if value <= warn {
                            let _ = self.tx.send(BalanceEvent::LowBalance(balance));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = self.tx.send(BalanceEvent::CheckFailed {
                        provider: provider.id(),
                        message: err.to_string(),
                    });
                }
            }
        }
    }
}

/// Parse DeepSeek-style balance JSON helper.
pub fn parse_deepseek_balance(provider: ProviderId, value: &serde_json::Value) -> AiResult<Balance> {
    let infos = value
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AiError::Serialization {
            message: "missing balance_infos".into(),
        })?;
    let first = infos.first().ok_or_else(|| AiError::Serialization {
        message: "empty balance_infos".into(),
    })?;
    let currency = first
        .get("currency")
        .and_then(|c| c.as_str())
        .unwrap_or("USD");
    let parse_dec = |key: &str| -> Option<Decimal> {
        first
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
    };
    let total = parse_dec("total_balance").unwrap_or(Decimal::ZERO);
    Ok(Balance {
        provider,
        currency: Currency::new(currency),
        total,
        available: Some(total),
        granted: parse_dec("granted_balance"),
        topped_up: parse_dec("topped_up_balance"),
        updated_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_deepseek_balance() {
        let v = json!({
            "balance_infos": [{
                "currency": "USD",
                "total_balance": "42.31",
                "granted_balance": "10.00",
                "topped_up_balance": "32.31"
            }]
        });
        let b = parse_deepseek_balance(ProviderId::deepseek(), &v).unwrap();
        assert_eq!(b.total, Decimal::new(4231, 2));
    }
}
