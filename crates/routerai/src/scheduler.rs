//! Scheduler: cron / interval / manual → actions.

use chrono::{DateTime, Utc};
use cron::Schedule as CronSchedule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::error::{RouterError, RouterResult};
use crate::handler::Action;
use crate::ids::ScheduleId;

/// How the schedule fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleKind {
    /// Cron expression (UTC).
    Cron {
        /// e.g. `0 */6 * * * *` (sec min hour day month dow) — cron crate 7-field with seconds.
        expr: String,
    },
    /// Fixed interval.
    Interval {
        /// Seconds.
        every_secs: u64,
    },
    /// Manual only.
    Manual,
}

/// Scheduled automation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Id.
    pub id: ScheduleId,
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: ScheduleKind,
    /// Action to run.
    pub action: Action,
    /// Enabled.
    pub enabled: bool,
    /// Last fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Created.
    pub created_at: DateTime<Utc>,
}

impl Schedule {
    /// Interval helper.
    pub fn every(name: impl Into<String>, every_secs: u64, action: Action) -> Self {
        Self {
            id: ScheduleId::new(),
            name: name.into(),
            kind: ScheduleKind::Interval { every_secs },
            action,
            enabled: true,
            last_fired_at: None,
            created_at: Utc::now(),
        }
    }

    /// Whether due now.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.kind {
            ScheduleKind::Manual => false,
            ScheduleKind::Interval { every_secs } => match self.last_fired_at {
                None => true,
                Some(last) => (now - last).num_seconds() as u64 >= *every_secs,
            },
            ScheduleKind::Cron { expr } => {
                let Ok(schedule) = CronSchedule::from_str(expr) else {
                    return false;
                };
                // Fire if a tick exists between last and now.
                let after = self.last_fired_at.unwrap_or(now - chrono::Duration::seconds(1));
                schedule.after(&after).next().is_some_and(|t| t <= now)
            }
        }
    }
}

/// Due work item.
#[derive(Debug, Clone)]
pub struct DueSchedule {
    /// Schedule.
    pub schedule: Schedule,
}

/// Scheduler registry.
#[derive(Clone, Default)]
pub struct Scheduler {
    items: Arc<RwLock<HashMap<String, Schedule>>>,
}

impl Scheduler {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert.
    pub async fn upsert(&self, schedule: Schedule) -> RouterResult<()> {
        // Validate cron early.
        if let ScheduleKind::Cron { expr } = &schedule.kind {
            CronSchedule::from_str(expr).map_err(|e| {
                RouterError::Invalid(format!("invalid cron `{expr}`: {e}"))
            })?;
        }
        if let ScheduleKind::Interval { every_secs } = schedule.kind {
            if every_secs == 0 {
                return Err(RouterError::Invalid("every_secs must be > 0".into()));
            }
        }
        self.items
            .write()
            .await
            .insert(schedule.id.to_string(), schedule);
        Ok(())
    }

    /// List.
    pub async fn list(&self) -> Vec<Schedule> {
        self.items.read().await.values().cloned().collect()
    }

    /// Mark fired; returns updated schedule when found.
    pub async fn mark_fired(&self, id: &ScheduleId, at: DateTime<Utc>) -> Option<Schedule> {
        if let Some(s) = self.items.write().await.get_mut(id.as_str()) {
            s.last_fired_at = Some(at);
            return Some(s.clone());
        }
        None
    }

    /// Collect due schedules.
    pub async fn due(&self, now: DateTime<Utc>) -> Vec<DueSchedule> {
        self.items
            .read()
            .await
            .values()
            .filter(|s| s.is_due(now))
            .map(|s| DueSchedule {
                schedule: s.clone(),
            })
            .collect()
    }

    /// Background tick loop handle.
    pub fn spawn_ticker<F, Fut>(self, interval: Duration, mut on_due: F) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(DueSchedule) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            loop {
                let now = Utc::now();
                let due = self.due(now).await;
                for item in due {
                    self.mark_fired(&item.schedule.id, now).await;
                    on_due(item).await;
                }
                tokio::time::sleep(interval).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AgentId;

    #[tokio::test]
    async fn interval_becomes_due() {
        let sched = Scheduler::new();
        let s = Schedule::every(
            "research",
            1,
            Action::AgentRun {
                agent_id: AgentId::from("market"),
            },
        );
        let id = s.id.clone();
        sched.upsert(s).await.unwrap();
        assert_eq!(sched.due(Utc::now()).await.len(), 1);
        sched.mark_fired(&id, Utc::now()).await;
        assert!(sched.due(Utc::now()).await.is_empty());
    }
}
