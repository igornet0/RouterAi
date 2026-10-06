import { useEffect, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Agent, AgentRun, agentReplyText, api, statusPillClass } from "../api";
import DebuggerPanel from "../components/DebuggerPanel";

export default function Debugger() {
  const { t, i18n } = useTranslation();
  const { id } = useParams();
  const [params] = useSearchParams();
  const runId = params.get("run");
  const [agent, setAgent] = useState<Agent | null>(null);
  const [runs, setRuns] = useState<AgentRun[]>([]);
  const [selected, setSelected] = useState<AgentRun | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    api.getAgent(id).then(setAgent).catch((e) => setError(String(e.message || e)));
    api
      .listRuns()
      .then((r) => {
        const mine = r.runs.filter((x) => x.agent_id === id);
        setRuns(mine);
        if (runId) {
          const found = mine.find((x) => x.id === runId);
          if (found) setSelected(found);
          else {
            api
              .getRun(runId)
              .then(setSelected)
              .catch((e) => setError(String(e.message || e)));
          }
        } else if (mine[0]) {
          setSelected(mine[0]);
        }
      })
      .catch((e) => setError(String(e.message || e)));
  }, [id, runId]);

  if (!agent) return <div className="empty">{t("debugger.loading")}</div>;

  const userMsg =
    selected &&
    typeof selected.input === "object" &&
    selected.input &&
    "message" in (selected.input as object)
      ? String((selected.input as { message?: string }).message ?? "")
      : selected
        ? JSON.stringify(selected.input, null, 2)
        : "";

  return (
    <>
      <div className="page-title">
        <div>
          <h1>
            {t("debugger.title")} · {agent.name}{" "}
            <span className={`pill ${statusPillClass(agent.status)}`}>
              {agent.status} · v{agent.version}
            </span>
          </h1>
          <p>
            {t("debugger.subtitle")}{" "}
            <Link to={`/agents/${agent.id}/playground`}>{t("debugger.playground")}</Link>
            {" · "}
            <Link to={`/agents/${agent.id}`}>{t("debugger.builder")}</Link>
          </p>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="debugger">
        <div className="panel">
          <h2>{t("debugger.runs")}</h2>
          <div className="run-pick">
            {runs.map((r) => (
              <button
                key={r.id}
                className={selected?.id === r.id ? "active" : ""}
                onClick={() => setSelected(r)}
              >
                {r.status} · ${r.cost} ·{" "}
                {new Date(r.started_at).toLocaleString(i18n.language)}
              </button>
            ))}
            {runs.length === 0 && <div className="empty">{t("debugger.noRuns")}</div>}
          </div>
        </div>
        <div className="panel debugger-convo">
          <h2>{t("debugger.conversation")}</h2>
          {!selected && <div className="empty">{t("debugger.selectRun")}</div>}
          {selected && (
            <>
              <div className="chat-bubble user">
                <div className="chat-role">{t("common.user")}</div>
                <div className="chat-text">{userMsg || t("debugger.emptyInput")}</div>
              </div>
              <div className="chat-bubble agent">
                <div className="chat-role">{t("common.agent")}</div>
                <div className="chat-text">{agentReplyText(selected)}</div>
              </div>
            </>
          )}
        </div>
        <div className="panel debugger-exec">
          <h2>{t("debugger.execution")}</h2>
          {selected ? (
            <DebuggerPanel run={selected} />
          ) : (
            <div className="empty">{t("common.emptyDash")}</div>
          )}
        </div>
      </div>
    </>
  );
}
