import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Agent, api, newDraftAgent, statusPillClass } from "../api";

export default function Agents() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<Agent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const nav = useNavigate();

  const load = () =>
    api
      .listAgents(true)
      .then((r) => setAgents(r.agents))
      .catch((e) => setError(String(e.message || e)));

  useEffect(() => {
    load();
  }, []);

  async function create() {
    try {
      const draft = newDraftAgent(t("agents.defaultName"));
      const created = await api.createAgent(draft);
      nav(`/agents/${created.id}`);
    } catch (e) {
      setError(String((e as Error).message || e));
    }
  }

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("agents.title")}</h1>
          <p>{t("agents.subtitle")}</p>
        </div>
        <div className="actions">
          <button className="primary" onClick={create}>
            {t("agents.create")}
          </button>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="panel">
        <table className="table">
          <thead>
            <tr>
              <th>{t("common.name")}</th>
              <th>{t("common.model")}</th>
              <th>{t("common.status")}</th>
              <th>{t("common.version")}</th>
              <th>{t("common.actions")}</th>
            </tr>
          </thead>
          <tbody>
            {agents.map((a) => (
              <tr key={a.id}>
                <td>
                  <Link to={`/agents/${a.id}`}>{a.name}</Link>
                </td>
                <td>
                  {a.model.provider}/{a.model.model}
                </td>
                <td>
                  <span className={`pill ${statusPillClass(a.status)}`}>
                    {a.status}
                  </span>
                </td>
                <td>v{a.version}</td>
                <td className="row-actions">
                  <Link to={`/agents/${a.id}`}>{t("common.edit")}</Link>
                  <Link to={`/agents/${a.id}/playground`}>{t("agents.playground")}</Link>
                  <Link to={`/agents/${a.id}/debugger`}>{t("agents.debugger")}</Link>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {agents.length === 0 && <div className="empty">{t("agents.noAgents")}</div>}
      </div>
    </>
  );
}
