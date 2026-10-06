import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { api } from "../api";

type Dash = {
  active_runs: number;
  events_last_hour: number;
  cost_today: string;
  failed_runs_today: number;
  kill_switch: boolean;
  agents: Array<{ id: string; name: string; status: string; version: number }>;
};

export default function Dashboard() {
  const { t } = useTranslation();
  const [data, setData] = useState<Dash | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .dashboard()
      .then((d) => setData(d as unknown as Dash))
      .catch((e) => setError(String(e.message || e)));
  }, []);

  if (error) return <div className="error-banner">{error}</div>;
  if (!data) return <div className="empty">{t("dashboard.loading")}</div>;

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("dashboard.title")}</h1>
          <p>{t("dashboard.subtitle")}</p>
        </div>
      </div>
      {data.kill_switch && (
        <div className="error-banner">{t("dashboard.killSwitch")}</div>
      )}
      <div className="grid-metrics">
        <div className="metric">
          <div className="label">{t("dashboard.activeRuns")}</div>
          <div className="value">{data.active_runs}</div>
        </div>
        <div className="metric">
          <div className="label">{t("dashboard.eventsPerHour")}</div>
          <div className="value">{data.events_last_hour}</div>
        </div>
        <div className="metric">
          <div className="label">{t("dashboard.costToday")}</div>
          <div className="value">${data.cost_today}</div>
        </div>
        <div className="metric">
          <div className="label">{t("dashboard.failedToday")}</div>
          <div className="value">{data.failed_runs_today}</div>
        </div>
      </div>
      <div className="panel">
        <h2>{t("dashboard.agents")}</h2>
        {data.agents.length === 0 ? (
          <div className="empty">
            {t("dashboard.noAgents")}{" "}
            <Link to="/agents">{t("dashboard.createOne")}</Link>
          </div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t("common.name")}</th>
                <th>{t("common.status")}</th>
                <th>{t("common.version")}</th>
              </tr>
            </thead>
            <tbody>
              {data.agents.map((a) => (
                <tr key={a.id}>
                  <td>
                    <Link to={`/agents/${a.id}`}>{a.name}</Link>
                  </td>
                  <td>
                    <span className={`pill ${a.status === "published" ? "ok" : "warn"}`}>
                      {a.status}
                    </span>
                  </td>
                  <td>v{a.version}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}
