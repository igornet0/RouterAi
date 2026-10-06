import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, PlatformEvent, WebhookTarget, subscribeEvents } from "../api";

export default function Events() {
  const { t, i18n } = useTranslation();
  const [events, setEvents] = useState<PlatformEvent[]>([]);
  const [selected, setSelected] = useState<PlatformEvent | null>(null);
  const [eventType, setEventType] = useState("webhook.message.received");
  const [payload, setPayload] = useState(
    '{\n  "customer": "123",\n  "message": "Хочу узнать цену"\n}',
  );
  const [mode, setMode] = useState<"emit" | "webhook">("webhook");
  const [targets, setTargets] = useState<WebhookTarget[]>([]);
  const [targetUrl, setTargetUrl] = useState("https://example.com/hooks/routerai");
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);

  const load = () =>
    api
      .listEvents()
      .then((r) => setEvents([...r.events].reverse()))
      .catch((e) => setError(String(e.message || e)));

  const loadTargets = () =>
    api
      .listWebhookTargets()
      .then((r) => setTargets(r.targets))
      .catch(() => undefined);

  useEffect(() => {
    load();
    loadTargets();
    return subscribeEvents((ev) => {
      setEvents((prev) => [ev, ...prev].slice(0, 200));
    });
  }, []);

  async function emit() {
    setError(null);
    setInfo(null);
    try {
      const body = JSON.parse(payload);
      if (mode === "webhook") {
        const type = eventType.replace(/^webhook\./, "") || "message.received";
        const r = await api.webhookIngress(type, body);
        setInfo(
          t("events.webhookInfo", {
            type: r.event.event_type,
            runs: r.runs.length,
          }),
        );
      } else {
        await api.emitEvent({
          event_type: eventType,
          source: "console",
          payload: body,
        });
      }
      load();
    } catch (e) {
      setError(String((e as Error).message || e));
    }
  }

  async function addTarget() {
    setError(null);
    try {
      await api.createWebhookTarget({
        url: targetUrl,
        event_types: ["agent.completed", "agent.failed"],
      });
      setInfo(t("events.sinkRegistered"));
      loadTargets();
    } catch (e) {
      setError(String((e as Error).message || e));
    }
  }

  async function removeTarget(id: string) {
    await api.deleteWebhookTarget(id);
    loadTargets();
  }

  const curl = `curl -sS -X POST http://127.0.0.1:8080/api/v1/webhooks/message.received \\
  -H 'Content-Type: application/json' \\
  -d '${payload.replace(/\n/g, " ").replace(/\s+/g, " ")}'`;

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("events.title")}</h1>
          <p>{t("events.subtitle")}</p>
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      {info && <div className="panel">{info}</div>}
      <div className="split">
        <div>
          <div className="panel">
            <h2>{t("events.ingress")}</h2>
            <div className="actions" style={{ marginBottom: "0.75rem" }}>
              <button
                className={mode === "webhook" ? "primary" : ""}
                onClick={() => {
                  setMode("webhook");
                  setEventType("webhook.message.received");
                }}
              >
                {t("events.webhook")}
              </button>
              <button
                className={mode === "emit" ? "primary" : ""}
                onClick={() => setMode("emit")}
              >
                {t("events.rawEmit")}
              </button>
            </div>
            <div className="field">
              <label>
                {mode === "webhook" ? t("events.pathType") : t("events.type")}
              </label>
              <input value={eventType} onChange={(e) => setEventType(e.target.value)} />
            </div>
            <div className="field">
              <label>{t("events.payload")}</label>
              <textarea value={payload} onChange={(e) => setPayload(e.target.value)} />
            </div>
            <button className="primary" onClick={emit}>
              {mode === "webhook" ? t("events.postWebhook") : t("events.emitEvent")}
            </button>
            {mode === "webhook" && (
              <div className="field" style={{ marginTop: "1rem" }}>
                <label>{t("events.curl")}</label>
                <pre className="pre tight">{curl}</pre>
              </div>
            )}
          </div>
          <div className="panel">
            <h2>{t("events.webhookSinks")}</h2>
            <p style={{ color: "var(--muted)", fontSize: "0.85rem" }}>
              {t("events.sinksHint")}
            </p>
            <div className="field">
              <label>{t("events.callbackUrl")}</label>
              <input value={targetUrl} onChange={(e) => setTargetUrl(e.target.value)} />
            </div>
            <button onClick={addTarget}>{t("events.registerSink")}</button>
            <ul className="sink-list">
              {targets.map((target) => (
                <li key={target.id}>
                  <code>{target.url}</code>
                  <span className="pill">{target.event_types.join(", ") || "*"}</span>
                  <button className="ghost" onClick={() => removeTarget(target.id)}>
                    {t("common.remove")}
                  </button>
                </li>
              ))}
            </ul>
            {targets.length === 0 && (
              <div className="empty">{t("events.noSinks")}</div>
            )}
          </div>
          <div className="panel event-stream">
            <h2>{t("events.stream")}</h2>
            {events.map((e) => (
              <div
                key={e.id}
                className="event-row"
                onClick={() => setSelected(e)}
              >
                <span className="time">
                  {new Date(e.timestamp).toLocaleTimeString(i18n.language)}
                </span>
                <span>{e.event_type}</span>
                <span className="pill">{e.source}</span>
              </div>
            ))}
          </div>
        </div>
        <div className="panel">
          <h2>{t("events.inspector")}</h2>
          {!selected && <div className="empty">{t("events.selectEvent")}</div>}
          {selected && (
            <>
              <p>
                <strong>{selected.event_type}</strong>
              </p>
              <p>
                ID <code>{selected.id}</code>
              </p>
              <p>
                {t("events.source")} {selected.source}
              </p>
              {selected.correlation_id && (
                <p>
                  {t("events.correlation")} {selected.correlation_id}
                </p>
              )}
              {selected.causation_id && (
                <p>
                  {t("events.causation")} {selected.causation_id}
                </p>
              )}
              <pre className="pre">{JSON.stringify(selected.payload, null, 2)}</pre>
            </>
          )}
        </div>
      </div>
    </>
  );
}
