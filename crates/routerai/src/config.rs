//! Runtime configuration.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// RouterAi runtime knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Global kill switch — refuse new runs.
    pub kill_switch: bool,
    /// Event history capacity.
    pub event_capacity: usize,
    /// Scheduler tick interval.
    #[serde(with = "humantime_ms")]
    pub scheduler_tick: Duration,
    /// Persist events to store.
    pub persist_events: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            kill_switch: false,
            event_capacity: 2048,
            scheduler_tick: Duration::from_secs(5),
            persist_events: true,
        }
    }
}

mod humantime_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(d: &Duration, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}
