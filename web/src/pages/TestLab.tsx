import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  Agent,
  AgentRun,
  api,
  EvaluationReport,
  TestCase,
} from "../api";
import TraceView from "../components/TraceView";

export default function TestLab() {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const [agents, setAgents] = useState<Agent[]>([]);
  const [agentId, setAgentId] = useState(params.get("agent") ?? "");
  const [message, setMessage] = useState("Хочу узнать стоимость вашего продукта");
  const [context, setContext] = useState('{\n  "customer_type": "new",\n  "language": "ru"\n}');
  const [mustContain, setMustContain] = useState("stub");
  const [mustNot, setMustNot] = useState("не знаю");
  const [cases, setCases] = useState<TestCase[]>([]);
  const [run, setRun] = useState<AgentRun | null>(null);
  const [evalReport, setEvalReport] = useState<EvaluationReport | null>(null);
  const [regression, setRegression] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.listAgents(true).then((r) => {
      setAgents(r.agents);
      if (!agentId && r.agents[0]) setAgentId(r.agents[0].id);
    });
  }, []);

  useEffect(() => {
    if (!agentId) return;
    api.listCases(agentId).then((r) => setCases(r.cases));
  }, [agentId]);

  const selected = useMemo(
    () => agents.find((a) => a.id === agentId),
    [agents, agentId],
  );

  async function runTest() {
    if (!agentId) return;
    setBusy(true);
    setError(null);
    setRegression(null);
    try {
      let ctx: Record<string, unknown> = {};
      try {
        ctx = JSON.parse(context);
      } catch {
        /* ignore */
      }
      const caseId = `case_${Date.now()}`;
      const testCase: TestCase = {
        id: caseId,
        agent_id: agentId,
        name: message.slice(0, 40),
        input: { message, ...ctx },
        expected: [
          { type: "must_succeed" },
          ...(mustContain
            ? [{ type: "must_contain", value: mustContain }]
            : []),
          ...(mustNot ? [{ type: "must_not_contain", value: mustNot }] : []),
        ],
      };
      await api.upsertCase(testCase);
      const result = await api.runCase(caseId);
      setRun(result.run);
      setEvalReport(result.evaluation);
      const refreshed = await api.listCases(agentId);
      setCases(refreshed.cases);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function runRegressionSuite() {
    if (!agentId || cases.length === 0) return;
    setBusy(true);
    setError(null);
    try {
      const dsId = `ds_${agentId}`;
      await api.upsertDataset({
        id: dsId,
        name: t("testLab.datasetName", { name: selected?.name ?? "Agent" }),
        agent_id: agentId,
        case_ids: cases.map((c) => c.id),
        created_at: new Date().toISOString(),
      });
      const report = await api.runRegression(dsId);
      setRegression(report);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("testLab.title")}</h1>
          <p>{t("testLab.subtitle")}</p>
        </div>
        <div className="actions">
          <button className="primary" disabled={busy || !agentId} onClick={runTest}>
            {t("testLab.runTest")}
          </button>
          <button disabled={busy || cases.length === 0} onClick={runRegressionSuite}>
            {t("testLab.regressionSuite")}
          </button>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="split">
        <div>
          <div className="panel">
            <div className="field">
              <label>{t("testLab.agent")}</label>
              <select value={agentId} onChange={(e) => setAgentId(e.target.value)}>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name} v{a.version} ({a.status})
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>{t("testLab.input")}</label>
              <textarea value={message} onChange={(e) => setMessage(e.target.value)} />
            </div>
            <div className="field">
              <label>{t("testLab.context")}</label>
              <textarea value={context} onChange={(e) => setContext(e.target.value)} />
            </div>
            <div className="field">
              <label>{t("testLab.mustContain")}</label>
              <input value={mustContain} onChange={(e) => setMustContain(e.target.value)} />
            </div>
            <div className="field">
              <label>{t("testLab.mustNotContain")}</label>
              <input value={mustNot} onChange={(e) => setMustNot(e.target.value)} />
            </div>
          </div>
          <div className="panel">
            <h2>{t("testLab.savedCases", { count: cases.length })}</h2>
            <table className="table">
              <thead>
                <tr>
                  <th>{t("common.id")}</th>
                  <th>{t("common.name")}</th>
                </tr>
              </thead>
              <tbody>
                {cases.map((c) => (
                  <tr key={c.id}>
                    <td>
                      <code>{c.id}</code>
                    </td>
                    <td>{c.name || t("common.emptyDash")}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
        <div>
          <div className="panel">
            <h2>{t("testLab.result")}</h2>
            {!run && <div className="empty">{t("testLab.runToSee")}</div>}
            {evalReport && (
              <div style={{ marginBottom: "0.75rem" }}>
                <span className={`pill ${evalReport.passed ? "ok" : "bad"}`}>
                  {evalReport.passed ? t("testLab.passed") : t("testLab.failed")}
                </span>{" "}
                {t("testLab.costLatency", {
                  cost: evalReport.cost,
                  ms: evalReport.latency_ms,
                })}
                <ul>
                  {evalReport.assertions.map((a, i) => (
                    <li key={i}>
                      {a.passed ? "✓" : "✗"} {a.message}
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {run && (
              <>
                <p>
                  {t("testLab.statusLine", {
                    status: run.status,
                    tokens: run.usage.total_tokens,
                    cost: run.cost,
                  })}
                </p>
                <TraceView steps={run.steps} />
                {run.output != null && (
                  <pre className="pre">{JSON.stringify(run.output, null, 2)}</pre>
                )}
              </>
            )}
          </div>
          {regression && (
            <div className="panel">
              <h2>{t("testLab.regression")}</h2>
              <pre className="pre">{JSON.stringify(regression, null, 2)}</pre>
            </div>
          )}
        </div>
      </div>
    </>
  );
}
