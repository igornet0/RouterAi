//! Test Lab — cases, assertions, datasets, regression.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::error::{RouterError, RouterResult};
use crate::ids::{AgentId, RunId};
use crate::run::{AgentRun, RunStatus};

/// Assertion against a completed run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    /// Output / text must contain substring.
    MustContain {
        /// Needle.
        value: String,
    },
    /// Must not contain.
    MustNotContain {
        /// Needle.
        value: String,
    },
    /// Max total cost.
    MaxCost {
        /// USD.
        amount: Decimal,
    },
    /// Max latency.
    MaxLatencyMs {
        /// Milliseconds.
        ms: u64,
    },
    /// Run must complete successfully.
    MustSucceed,
}

/// A test case for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    /// Id.
    pub id: String,
    /// Agent under test.
    pub agent_id: AgentId,
    /// Input payload (becomes run input).
    pub input: Value,
    /// Assertions.
    pub expected: Vec<Assertion>,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Optional dataset id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
}

/// Named collection of test case ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Agent lineage / version this suite targets.
    pub agent_id: AgentId,
    /// Case ids.
    #[serde(default)]
    pub case_ids: Vec<String>,
    /// Created.
    pub created_at: DateTime<Utc>,
}

/// Single assertion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    /// Assertion.
    pub assertion: Assertion,
    /// Passed?
    pub passed: bool,
    /// Detail.
    pub message: String,
}

/// Evaluation report for one test case run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReport {
    /// Test case id.
    pub test_case_id: String,
    /// Run id.
    pub run_id: RunId,
    /// Overall pass.
    pub passed: bool,
    /// Per assertion.
    pub assertions: Vec<AssertionResult>,
    /// Cost.
    pub cost: Decimal,
    /// Latency.
    pub latency_ms: u64,
    /// When evaluated.
    pub evaluated_at: DateTime<Utc>,
}

/// Aggregate regression metrics (factual — no invented quality score).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionReport {
    /// Dataset id.
    pub dataset_id: String,
    /// Agent id used.
    pub agent_id: AgentId,
    /// Total cases.
    pub total: u64,
    /// Passed.
    pub passed: u64,
    /// Failed.
    pub failed: u64,
    /// Average cost USD.
    pub average_cost: Decimal,
    /// Average latency ms.
    pub average_latency_ms: f64,
    /// Tool errors observed in traces.
    pub tool_errors: u64,
    /// Per-case reports.
    pub evaluations: Vec<EvaluationReport>,
    /// When finished.
    pub finished_at: DateTime<Utc>,
}

/// In-memory test case / dataset registry.
#[derive(Clone, Default)]
pub struct TestCaseStore {
    cases: Arc<RwLock<HashMap<String, TestCase>>>,
    datasets: Arc<RwLock<HashMap<String, Dataset>>>,
}

impl TestCaseStore {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert case.
    pub async fn upsert_case(&self, case: TestCase) -> RouterResult<TestCase> {
        self.cases
            .write()
            .await
            .insert(case.id.clone(), case.clone());
        Ok(case)
    }

    /// Get case.
    pub async fn get_case(&self, id: &str) -> Option<TestCase> {
        self.cases.read().await.get(id).cloned()
    }

    /// List cases (optionally by agent).
    pub async fn list_cases(&self, agent_id: Option<&AgentId>) -> Vec<TestCase> {
        self.cases
            .read()
            .await
            .values()
            .filter(|c| agent_id.map(|a| &c.agent_id == a).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Delete case.
    pub async fn remove_case(&self, id: &str) -> RouterResult<()> {
        if self.cases.write().await.remove(id).is_none() {
            return Err(RouterError::NotFound(format!("test case {id}")));
        }
        Ok(())
    }

    /// Upsert dataset.
    pub async fn upsert_dataset(&self, ds: Dataset) -> RouterResult<Dataset> {
        self.datasets
            .write()
            .await
            .insert(ds.id.clone(), ds.clone());
        Ok(ds)
    }

    /// List datasets.
    pub async fn list_datasets(&self) -> Vec<Dataset> {
        self.datasets.read().await.values().cloned().collect()
    }

    /// Get dataset.
    pub async fn get_dataset(&self, id: &str) -> Option<Dataset> {
        self.datasets.read().await.get(id).cloned()
    }
}

/// Test lab helpers.
pub struct TestLab;

impl TestLab {
    /// Evaluate a finished run against a case.
    pub fn evaluate(case: &TestCase, run: &AgentRun) -> EvaluationReport {
        let text = run
            .output
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let latency_ms = run
            .finished_at
            .map(|f| (f - run.started_at).num_milliseconds().max(0) as u64)
            .unwrap_or(0);

        let mut results = Vec::new();
        for a in &case.expected {
            let (passed, message) = match a {
                Assertion::MustContain { value } => {
                    let ok = text.contains(value);
                    (ok, format!("must_contain `{value}` → {ok}"))
                }
                Assertion::MustNotContain { value } => {
                    let ok = !text.contains(value);
                    (ok, format!("must_not_contain `{value}` → {ok}"))
                }
                Assertion::MaxCost { amount } => {
                    let ok = run.cost <= *amount;
                    (ok, format!("cost {} <= {amount} → {ok}", run.cost))
                }
                Assertion::MaxLatencyMs { ms } => {
                    let ok = latency_ms <= *ms;
                    (ok, format!("latency {latency_ms}ms <= {ms} → {ok}"))
                }
                Assertion::MustSucceed => {
                    let ok = run.status == RunStatus::Completed;
                    (ok, format!("status {:?} → {ok}", run.status))
                }
            };
            results.push(AssertionResult {
                assertion: a.clone(),
                passed,
                message,
            });
        }

        let passed = results.iter().all(|r| r.passed);
        EvaluationReport {
            test_case_id: case.id.clone(),
            run_id: run.id.clone(),
            passed,
            assertions: results,
            cost: run.cost,
            latency_ms,
            evaluated_at: Utc::now(),
        }
    }

    /// Aggregate factual regression metrics.
    pub fn aggregate_regression(
        dataset_id: &str,
        agent_id: AgentId,
        pairs: &[(AgentRun, EvaluationReport)],
    ) -> RegressionReport {
        let total = pairs.len() as u64;
        let passed = pairs.iter().filter(|(_, e)| e.passed).count() as u64;
        let failed = total.saturating_sub(passed);
        let sum_cost: Decimal = pairs.iter().map(|(_, e)| e.cost).sum();
        let average_cost = if total > 0 {
            sum_cost / Decimal::from(total)
        } else {
            Decimal::ZERO
        };
        let average_latency_ms = if total > 0 {
            pairs.iter().map(|(_, e)| e.latency_ms as f64).sum::<f64>() / total as f64
        } else {
            0.0
        };
        let tool_errors = pairs
            .iter()
            .map(|(run, _)| {
                run.steps
                    .iter()
                    .filter(|s| {
                        matches!(s.kind, crate::run::RunStepKind::Error)
                            && s.summary.to_ascii_lowercase().contains("tool")
                    })
                    .count() as u64
            })
            .sum();

        RegressionReport {
            dataset_id: dataset_id.to_string(),
            agent_id,
            total,
            passed,
            failed,
            average_cost,
            average_latency_ms,
            tool_errors,
            evaluations: pairs.iter().map(|(_, e)| e.clone()).collect(),
            finished_at: Utc::now(),
        }
    }

    /// Convenience timeout for test runs.
    pub fn default_timeout() -> Duration {
        Duration::from_secs(60)
    }
}

/// Builder for a simple text assertion case.
pub fn text_case(
    id: impl Into<String>,
    agent_id: AgentId,
    message: impl Into<String>,
    must_contain: &[&str],
    must_not_contain: &[&str],
) -> TestCase {
    let mut expected = vec![Assertion::MustSucceed];
    for v in must_contain {
        expected.push(Assertion::MustContain {
            value: (*v).to_string(),
        });
    }
    for v in must_not_contain {
        expected.push(Assertion::MustNotContain {
            value: (*v).to_string(),
        });
    }
    TestCase {
        id: id.into(),
        agent_id,
        input: serde_json::json!({ "message": message.into() }),
        expected,
        name: String::new(),
        dataset_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AgentId;
    use rust_decimal::Decimal;

    #[test]
    fn evaluates_contains() {
        let agent = AgentId::from("sales");
        let case = text_case("t1", agent.clone(), "price?", &["цена"], &["не знаю"]);
        let mut run = AgentRun::new(agent, case.input.clone(), None);
        run.complete(serde_json::json!({"text": "Наша цена 100$"}));
        let report = TestLab::evaluate(&case, &run);
        assert!(report.passed);
        assert_eq!(report.cost, Decimal::ZERO);
    }

    #[test]
    fn regression_aggregates_facts() {
        let agent = AgentId::from("sales");
        let case = text_case("t1", agent.clone(), "x", &[], &[]);
        let mut run = AgentRun::new(agent.clone(), case.input.clone(), None);
        run.complete(serde_json::json!({"text": "ok"}));
        run.cost = Decimal::new(42, 4);
        let ev = TestLab::evaluate(&case, &run);
        let report = TestLab::aggregate_regression("ds1", agent, &[(run, ev)]);
        assert_eq!(report.total, 1);
        assert_eq!(report.passed, 1);
        assert_eq!(report.average_cost, Decimal::new(42, 4));
    }
}
