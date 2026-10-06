import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Agent, AgentRun, agentReplyText, api, statusPillClass } from "../api";
import DebuggerPanel from "../components/DebuggerPanel";

type ChatTurn = {
  role: "user" | "agent";
  text: string;
  run?: AgentRun;
};

export default function Playground() {
  const { t } = useTranslation();
  const { id } = useParams();
  const [agent, setAgent] = useState<Agent | null>(null);
  const [turns, setTurns] = useState<ChatTurn[]>([]);
  const [message, setMessage] = useState("");
  const [activeRun, setActiveRun] = useState<AgentRun | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!id) return;
    api.getAgent(id).then(setAgent).catch((e) => setError(String(e.message || e)));
  }, [id]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [turns]);

  async function send() {
    if (!agent || !message.trim() || busy) return;
    const text = message.trim();
    setMessage("");
    setTurns((prev) => [...prev, { role: "user", text }]);
    setBusy(true);
    setError(null);
    try {
      const run = await api.playground(agent.id, text);
      setActiveRun(run);
      setTurns((prev) => [
        ...prev,
        { role: "agent", text: agentReplyText(run), run },
      ]);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  if (!agent) return <div className="empty">{t("playground.loading")}</div>;

  const latencyMs =
    activeRun?.finished_at && activeRun.started_at
      ? Math.max(
          0,
          new Date(activeRun.finished_at).getTime() -
            new Date(activeRun.started_at).getTime(),
        )
      : null;

  return (
    <>
      <div className="page-title">
        <div>
          <h1>
            {t("playground.title")} · {agent.name}{" "}
            <span className={`pill ${statusPillClass(agent.status)}`}>
              {agent.status} · v{agent.version}
            </span>
          </h1>
          <p>
            {t("playground.subtitle")}{" "}
            <Link to={`/agents/${agent.id}`}>{t("playground.builder")}</Link>
            {" · "}
            <Link to={`/agents/${agent.id}/debugger`}>{t("playground.debugger")}</Link>
          </p>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="playground">
        <div className="panel playground-chat">
          <div className="chat-header">
            {agent.name} v{agent.version}
          </div>
          <div className="chat-body">
            {turns.length === 0 && (
              <div className="empty">{t("playground.emptyChat")}</div>
            )}
            {turns.map((turn, i) => (
              <div key={i} className={`chat-bubble ${turn.role}`}>
                <div className="chat-role">
                  {turn.role === "user" ? t("common.user") : t("common.agent")}
                </div>
                <div className="chat-text">{turn.text}</div>
                {turn.run && (
                  <button
                    className="ghost chat-trace-btn"
                    onClick={() => setActiveRun(turn.run!)}
                  >
                    {t("playground.showTrace")}
                  </button>
                )}
              </div>
            ))}
            <div ref={bottomRef} />
          </div>
          <div className="chat-input">
            <input
              value={message}
              placeholder={t("playground.messagePlaceholder")}
              disabled={busy}
              onChange={(e) => setMessage(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
            <button
              className="primary"
              disabled={busy || !message.trim()}
              onClick={send}
            >
              {t("playground.send")}
            </button>
          </div>
        </div>
        <div className="panel playground-trace">
          <h2>{t("playground.executionTrace")}</h2>
          {!activeRun && <div className="empty">{t("playground.noRun")}</div>}
          {activeRun && (
            <>
              <div className="trace-meta">
                <div>
                  {t("playground.cost")} <strong>${activeRun.cost}</strong>
                </div>
                <div>
                  {t("playground.latency")}{" "}
                  <strong>
                    {latencyMs != null
                      ? `${(latencyMs / 1000).toFixed(2)}s`
                      : t("common.emptyDash")}
                  </strong>
                </div>
                <div>
                  {t("playground.tokens")}{" "}
                  <strong>{activeRun.usage.total_tokens}</strong>
                </div>
                <div>
                  {t("playground.status")} <strong>{activeRun.status}</strong>
                </div>
              </div>
              <DebuggerPanel run={activeRun} />
            </>
          )}
        </div>
      </div>
    </>
  );
}
