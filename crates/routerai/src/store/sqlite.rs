//! SQLite persistence for RouterAi.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::adapter::SinkTarget;
use crate::agent::Agent;
use crate::audit::AuditEntry;
use crate::error::RouterResult;
use crate::eval::{Dataset, TestCase};
use crate::event::Event;
use crate::handler::Handler;
use crate::ids::{AgentId, EventId, HandlerId, RunId, ScheduleId};
use crate::run::AgentRun;
use crate::scheduler::Schedule;
use crate::store::{storage_err, Store};

/// SQLite-backed store.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Connect and migrate.
    pub async fn connect(url: &str) -> RouterResult<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(storage_err)?;
        let s = Self { pool };
        s.migrate().await?;
        Ok(s)
    }

    async fn migrate(&self) -> RouterResult<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS handlers (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                started_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS schedules (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sink_targets (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS test_cases (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS datasets (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_log (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(json: &str) -> RouterResult<T> {
    serde_json::from_str(json).map_err(storage_err)
}

#[async_trait]
impl Store for SqliteStore {
    async fn save_event(&self, event: &Event) -> RouterResult<()> {
        let json = serde_json::to_string(event).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO events (id, json, created_at) VALUES (?, ?, ?)")
            .bind(event.id.to_string())
            .bind(json)
            .bind(event.timestamp.to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_events(&self, limit: usize) -> RouterResult<Vec<Event>> {
        let rows = sqlx::query("SELECT json FROM events ORDER BY created_at DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(decode_json(&row.get::<String, _>("json"))?);
        }
        out.reverse();
        Ok(out)
    }

    async fn get_event(&self, id: &EventId) -> RouterResult<Option<Event>> {
        let row = sqlx::query("SELECT json FROM events WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(match row {
            Some(r) => Some(decode_json(&r.get::<String, _>("json"))?),
            None => None,
        })
    }

    async fn save_handler(&self, handler: &Handler) -> RouterResult<()> {
        let json = serde_json::to_string(handler).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO handlers (id, json) VALUES (?, ?)")
            .bind(handler.id.to_string())
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_handlers(&self) -> RouterResult<Vec<Handler>> {
        let rows = sqlx::query("SELECT json FROM handlers")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn get_handler(&self, id: &HandlerId) -> RouterResult<Option<Handler>> {
        let row = sqlx::query("SELECT json FROM handlers WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(match row {
            Some(r) => Some(decode_json(&r.get::<String, _>("json"))?),
            None => None,
        })
    }

    async fn delete_handler(&self, id: &HandlerId) -> RouterResult<()> {
        sqlx::query("DELETE FROM handlers WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_agent(&self, agent: &Agent) -> RouterResult<()> {
        let json = serde_json::to_string(agent).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO agents (id, json) VALUES (?, ?)")
            .bind(agent.id.to_string())
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_agents(&self) -> RouterResult<Vec<Agent>> {
        let rows = sqlx::query("SELECT json FROM agents")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn get_agent(&self, id: &AgentId) -> RouterResult<Option<Agent>> {
        let row = sqlx::query("SELECT json FROM agents WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(match row {
            Some(r) => Some(decode_json(&r.get::<String, _>("json"))?),
            None => None,
        })
    }

    async fn delete_agent(&self, id: &AgentId) -> RouterResult<()> {
        sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_run(&self, run: &AgentRun) -> RouterResult<()> {
        let json = serde_json::to_string(run).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO runs (id, json, started_at) VALUES (?, ?, ?)")
            .bind(run.id.to_string())
            .bind(json)
            .bind(run.started_at.to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_runs(&self, limit: usize) -> RouterResult<Vec<AgentRun>> {
        let rows = sqlx::query("SELECT json FROM runs ORDER BY started_at DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn get_run(&self, id: &RunId) -> RouterResult<Option<AgentRun>> {
        let row = sqlx::query("SELECT json FROM runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(match row {
            Some(r) => Some(decode_json(&r.get::<String, _>("json"))?),
            None => None,
        })
    }

    async fn save_schedule(&self, schedule: &Schedule) -> RouterResult<()> {
        let json = serde_json::to_string(schedule).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO schedules (id, json) VALUES (?, ?)")
            .bind(schedule.id.to_string())
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_schedules(&self) -> RouterResult<Vec<Schedule>> {
        let rows = sqlx::query("SELECT json FROM schedules")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn delete_schedule(&self, id: &ScheduleId) -> RouterResult<()> {
        sqlx::query("DELETE FROM schedules WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_sink_target(&self, target: &SinkTarget) -> RouterResult<()> {
        let json = serde_json::to_string(target).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO sink_targets (id, json) VALUES (?, ?)")
            .bind(&target.id)
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_sink_targets(&self) -> RouterResult<Vec<SinkTarget>> {
        let rows = sqlx::query("SELECT json FROM sink_targets")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn delete_sink_target(&self, id: &str) -> RouterResult<()> {
        sqlx::query("DELETE FROM sink_targets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_test_case(&self, case: &TestCase) -> RouterResult<()> {
        let json = serde_json::to_string(case).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO test_cases (id, json) VALUES (?, ?)")
            .bind(&case.id)
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_test_cases(&self) -> RouterResult<Vec<TestCase>> {
        let rows = sqlx::query("SELECT json FROM test_cases")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn delete_test_case(&self, id: &str) -> RouterResult<()> {
        sqlx::query("DELETE FROM test_cases WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_dataset(&self, dataset: &Dataset) -> RouterResult<()> {
        let json = serde_json::to_string(dataset).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO datasets (id, json) VALUES (?, ?)")
            .bind(&dataset.id)
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_datasets(&self) -> RouterResult<Vec<Dataset>> {
        let rows = sqlx::query("SELECT json FROM datasets")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn delete_dataset(&self, id: &str) -> RouterResult<()> {
        sqlx::query("DELETE FROM datasets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn save_audit(&self, entry: &AuditEntry) -> RouterResult<()> {
        let json = serde_json::to_string(entry).map_err(storage_err)?;
        sqlx::query("INSERT OR REPLACE INTO audit_log (id, json, at) VALUES (?, ?, ?)")
            .bind(&entry.id)
            .bind(json)
            .bind(entry.at.to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_audit(&self, limit: usize) -> RouterResult<Vec<AuditEntry>> {
        let rows = sqlx::query("SELECT json FROM audit_log ORDER BY at DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.into_iter()
            .map(|r| decode_json(&r.get::<String, _>("json")))
            .collect()
    }

    async fn get_setting(&self, key: &str) -> RouterResult<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    async fn set_setting(&self, key: &str, value: &str) -> RouterResult<()> {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }
}
