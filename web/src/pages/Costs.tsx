import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AgentRun, api } from "../api";

export default function Costs() {
  const { t } = useTranslation();
  const [runs, setRuns] = useState<AgentRun[]>([]);
  const [dash, setDash] = useState<Record<string, unknown> | null>(null);

  useEffect(() => {
    api.listRuns().then((r) => setRuns(r.runs));
    api.dashboard().then(setDash);
  }, []);

  const total = runs.reduce((acc, r) => acc + Number(r.cost || 0), 0);

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("costs.title")}</h1>
          <p>{t("costs.subtitle")}</p>
        </div>
      </div>
      <div className="grid-metrics">
        <div className="metric">
          <div className="label">{t("costs.costToday")}</div>
          <div className="value">${String(dash?.cost_today ?? "0")}</div>
        </div>
        <div className="metric">
          <div className="label">{t("costs.listedTotal")}</div>
          <div className="value">${total.toFixed(4)}</div>
        </div>
        <div className="metric">
          <div className="label">{t("costs.runs")}</div>
          <div className="value">{runs.length}</div>
        </div>
        <div className="metric">
          <div className="label">{t("costs.tokensListed")}</div>
          <div className="value">
            {runs.reduce((a, r) => a + (r.usage?.total_tokens ?? 0), 0)}
          </div>
        </div>
      </div>
      <div className="panel">
        <table className="table">
          <thead>
            <tr>
              <th>{t("costs.run")}</th>
              <th>{t("common.agent")}</th>
              <th>{t("common.tokens")}</th>
              <th>{t("common.cost")}</th>
            </tr>
          </thead>
          <tbody>
            {runs.map((r) => (
              <tr key={r.id}>
                <td>
                  <code>{r.id}</code>
                </td>
                <td>{r.agent_id}</td>
                <td>{r.usage.total_tokens}</td>
                <td>${r.cost}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
