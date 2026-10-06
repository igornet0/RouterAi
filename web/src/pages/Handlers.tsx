import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Agent, api, Handler } from "../api";

export default function Handlers() {
  const { t } = useTranslation();
  const [handlers, setHandlers] = useState<Handler[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [name, setName] = useState("");
  const [eventType, setEventType] = useState("telegram.message.received");
  const [agentId, setAgentId] = useState("");
  const [emitType, setEmitType] = useState("sales.lead.detected");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setName(t("handlers.defaultName"));
  }, [t]);

  const load = () => {
    api.listHandlers().then((r) => setHandlers(r.handlers));
    api.listAgents(true).then((r) => {
      setAgents(r.agents);
      if (!agentId && r.agents[0]) setAgentId(r.agents[0].id);
    });
  };

  useEffect(() => {
    load();
  }, []);

  async function create() {
    if (!agentId) return;
    setError(null);
    const now = new Date().toISOString();
    const handler: Handler = {
      id: `hdl_${Math.random().toString(16).slice(2, 10)}`,
      name,
      enabled: true,
      trigger: { event: eventType },
      conditions: [
        { field: "payload.text", operator: "exists" },
      ],
      actions: [
        { type: "agent_run", agent_id: agentId },
        { type: "event_emit", event_type: emitType, source: "handler" },
      ],
      max_retries: 3,
      timeout_secs: 60,
      concurrency: 5,
      error_policy: "fail_fast",
      created_at: now,
      updated_at: now,
    };
    try {
      await api.upsertHandler(handler);
      load();
    } catch (e) {
      setError(String((e as Error).message || e));
    }
  }

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("handlers.title")}</h1>
          <p>{t("handlers.subtitle")}</p>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="split">
        <div className="panel">
          <h2>{t("handlers.builder")}</h2>
          <div className="field">
            <label>{t("handlers.name")}</label>
            <input value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="field">
            <label>{t("handlers.whenEvent")}</label>
            <input value={eventType} onChange={(e) => setEventType(e.target.value)} />
          </div>
          <div className="field">
            <label>{t("handlers.thenAgent")}</label>
            <select value={agentId} onChange={(e) => setAgentId(e.target.value)}>
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name} ({a.status})
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label>{t("handlers.andEmit")}</label>
            <input value={emitType} onChange={(e) => setEmitType(e.target.value)} />
          </div>
          <p style={{ color: "var(--muted)", fontSize: "0.85rem" }}>
            {t("handlers.defaultsHint")}
          </p>
          <button className="primary" onClick={create}>
            {t("handlers.save")}
          </button>
        </div>
        <div className="panel">
          <h2>{t("handlers.active")}</h2>
          <table className="table">
            <thead>
              <tr>
                <th>{t("common.name")}</th>
                <th>{t("handlers.trigger")}</th>
                <th>{t("handlers.enabled")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {handlers.map((h) => (
                <tr key={h.id}>
                  <td>{h.name}</td>
                  <td>
                    <code>{h.trigger.event}</code>
                  </td>
                  <td>
                    <span className={`pill ${h.enabled ? "ok" : "warn"}`}>
                      {h.enabled ? t("handlers.statusActive") : t("handlers.statusOff")}
                    </span>
                  </td>
                  <td>
                    <button
                      className="ghost"
                      onClick={() => api.deleteHandler(h.id).then(load)}
                    >
                      {t("common.delete")}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </>
  );
}
