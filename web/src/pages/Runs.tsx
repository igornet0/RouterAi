import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { AgentRun, api, subscribeEvents } from "../api";
import TraceView from "../components/TraceView";

export default function Runs() {
  const { t } = useTranslation();
  const { id } = useParams();
  const [runs, setRuns] = useState<AgentRun[]>([]);
  const [selected, setSelected] = useState<AgentRun | null>(null);
  const [live, setLive] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    api
      .listRuns()
      .then((r) => {
        setRuns(r.runs);
        if (id) {
          const found = r.runs.find((x) => x.id === id);
          if (found) setSelected(found);
          else api.getRun(id).then(setSelected).catch(() => undefined);
        }
      })
      .catch((e) => setError(String(e.message || e)));

  useEffect(() => {
    load();
    const unsub = subscribeEvents((ev) => {
      setLive((prev) => [`${ev.timestamp} ${ev.event_type}`, ...prev].slice(0, 30));
      if (
        ev.event_type === "agent.completed" ||
        ev.event_type === "agent.failed"
      ) {
        load();
      }
    });
    return unsub;
  }, [id]);

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("runs.title")}</h1>
          <p>{t("runs.subtitle")}</p>
        </div>
        <button onClick={load}>{t("common.refresh")}</button>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="split">
        <div className="panel">
          <h2>{t("runs.history")}</h2>
          <table className="table">
            <thead>
              <tr>
                <th>{t("common.id")}</th>
                <th>{t("common.agent")}</th>
                <th>{t("common.status")}</th>
                <th>{t("common.cost")}</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => (
                <tr key={r.id} onClick={() => setSelected(r)} style={{ cursor: "pointer" }}>
                  <td>
                    <Link to={`/runs/${r.id}`}>{r.id}</Link>
                  </td>
                  <td>{r.agent_id}</td>
                  <td>
                    <span
                      className={`pill ${
                        r.status === "completed"
                          ? "ok"
                          : r.status === "failed"
                            ? "bad"
                            : "live"
                      }`}
                    >
                      {r.status}
                    </span>
                  </td>
                  <td>${r.cost}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <div>
          <div className="panel">
            <h2>{t("runs.selected")}</h2>
            {!selected && <div className="empty">{t("runs.selectRun")}</div>}
            {selected && (
              <>
                <p>
                  {t("runs.runMeta", {
                    status: selected.status,
                    cost: selected.cost,
                    tokens: selected.usage.total_tokens,
                  })}
                </p>
                <TraceView steps={selected.steps} />
                {selected.output != null && (
                  <pre className="pre">{JSON.stringify(selected.output, null, 2)}</pre>
                )}
                {selected.error != null && (
                  <pre className="pre">{JSON.stringify(selected.error, null, 2)}</pre>
                )}
              </>
            )}
          </div>
          <div className="panel">
            <h2>{t("runs.liveSse")}</h2>
            <div className="event-stream">
              {live.length === 0 && <div className="empty">{t("runs.waiting")}</div>}
              {live.map((line, i) => (
                <div key={i} className="event-row">
                  <span className="time">{line}</span>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>
    </>
  );
}
