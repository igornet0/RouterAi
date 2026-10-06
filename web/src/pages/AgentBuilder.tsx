import { useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  Agent,
  ProviderDescriptor,
  PublishValidationReport,
  api,
  statusPillClass,
} from "../api";

const TAB_KEYS = [
  "instructions",
  "model",
  "tools",
  "limits",
  "permissions",
  "lifecycle",
] as const;

type TabKey = (typeof TAB_KEYS)[number];

export default function AgentBuilder() {
  const { t, i18n } = useTranslation();
  const { id } = useParams();
  const nav = useNavigate();
  const [agent, setAgent] = useState<Agent | null>(null);
  const [tab, setTab] = useState<TabKey>("instructions");
  const [tools, setTools] = useState<string[]>([]);
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [validation, setValidation] = useState<PublishValidationReport | null>(null);
  const [overrideTests, setOverrideTests] = useState(false);

  useEffect(() => {
    if (!id) return;
    api
      .getAgent(id)
      .then(setAgent)
      .catch((e) => setError(String(e.message || e)));
    api.listTools().then((r) => setTools(r.tools.map((tool) => tool.id)));
    api.listProviders().then((r) => setProviders(r.providers)).catch(() => {});
  }, [id]);

  async function save() {
    if (!agent) return;
    setBusy(true);
    setError(null);
    try {
      const saved = await api.updateAgent(agent.id, {
        ...agent,
        updated_at: new Date().toISOString(),
      });
      setAgent(saved);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function validate() {
    if (!agent) return;
    setBusy(true);
    setError(null);
    try {
      await save();
      const report = await api.validateAgent(agent.id);
      setValidation(report);
      setTab("lifecycle");
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function publish() {
    if (!agent) return;
    setBusy(true);
    setError(null);
    try {
      await save();
      const result = await api.publishAgent(agent.id, overrideTests);
      if (result.agent) setAgent(result.agent);
      setValidation(result.validation);
      if (result.override_used) {
        setError(t("builder.overridePublished"));
      }
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function revise() {
    if (!agent) return;
    setBusy(true);
    try {
      const next = await api.reviseAgent(agent.id);
      nav(`/agents/${next.id}`);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function control(action: "pause" | "resume" | "archive" | "testing" | "ready") {
    if (!agent) return;
    setBusy(true);
    setError(null);
    try {
      const fn = {
        pause: api.pauseAgent,
        resume: api.resumeAgent,
        archive: api.archiveAgent,
        testing: api.markTesting,
        ready: api.markReady,
      }[action];
      const next = await fn(agent.id);
      setAgent(next);
    } catch (e) {
      setError(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  if (!agent) return <div className="empty">{t("builder.loading")}</div>;

  return (
    <>
      <div className="page-title">
        <div>
          <h1>
            {agent.name}{" "}
            <span className={`pill ${statusPillClass(agent.status)}`}>
              {agent.status} · v{agent.version}
            </span>
          </h1>
          <p>
            {t("builder.lineage")} <code>{agent.lineage_id}</code> ·{" "}
            {t("builder.lifecycleHint")}
          </p>
        </div>
        <div className="actions">
          <button disabled={busy} onClick={() => nav(`/agents/${agent.id}/playground`)}>
            {t("builder.playground")}
          </button>
          <button disabled={busy} onClick={() => nav(`/agents/${agent.id}/debugger`)}>
            {t("builder.debugger")}
          </button>
          <button disabled={busy} onClick={() => nav(`/test-lab?agent=${agent.id}`)}>
            {t("builder.testLab")}
          </button>
          <button disabled={busy} onClick={validate}>
            {t("builder.validate")}
          </button>
          <button disabled={busy} onClick={save}>
            {t("builder.save")}
          </button>
          {agent.status === "published" ? (
            <>
              <button disabled={busy} onClick={() => control("pause")}>
                {t("builder.pause")}
              </button>
              <button disabled={busy} onClick={revise}>
                {t("builder.newVersion")}
              </button>
            </>
          ) : agent.status === "paused" ? (
            <button className="primary" disabled={busy} onClick={() => control("resume")}>
              {t("builder.resume")}
            </button>
          ) : (
            <button className="primary" disabled={busy} onClick={publish}>
              {t("builder.publish")}
            </button>
          )}
        </div>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className="builder">
        <div className="builder-nav panel">
          {TAB_KEYS.map((key) => (
            <button
              key={key}
              className={tab === key ? "active" : ""}
              onClick={() => setTab(key)}
            >
              {t(`builder.tabs.${key}`)}
            </button>
          ))}
        </div>
        <div className="panel">
          {tab === "instructions" && (
            <>
              <div className="field">
                <label>{t("builder.name")}</label>
                <input
                  value={agent.name}
                  onChange={(e) => setAgent({ ...agent, name: e.target.value })}
                />
              </div>
              <div className="field">
                <label>{t("builder.systemInstructions")}</label>
                <textarea
                  value={agent.instructions}
                  onChange={(e) =>
                    setAgent({ ...agent, instructions: e.target.value })
                  }
                />
              </div>
            </>
          )}
          {tab === "model" && (
            <>
              <div className="field">
                <label>{t("builder.provider")}</label>
                <select
                  value={agent.model.provider}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      model: { ...agent.model, provider: e.target.value },
                    })
                  }
                >
                  {providers.length === 0 && (
                    <option value={agent.model.provider}>
                      {agent.model.provider}
                    </option>
                  )}
                  {providers.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                  {!providers.some((p) => p.id === agent.model.provider) &&
                    agent.model.provider && (
                      <option value={agent.model.provider}>
                        {t("builder.customProvider", { id: agent.model.provider })}
                      </option>
                    )}
                </select>
              </div>
              <div className="field">
                <label>{t("common.model")}</label>
                <input
                  value={agent.model.model}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      model: { ...agent.model, model: e.target.value },
                    })
                  }
                />
              </div>
              <div className="field">
                <label>{t("builder.temperature")}</label>
                <input
                  type="number"
                  step="0.1"
                  value={agent.model.temperature ?? 0.2}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      model: {
                        ...agent.model,
                        temperature: Number(e.target.value),
                      },
                    })
                  }
                />
              </div>
            </>
          )}
          {tab === "tools" && (
            <div className="tool-chips">
              {tools.map((toolId) => {
                const on = agent.tools.includes(toolId);
                return (
                  <button
                    key={toolId}
                    className={`tool-chip ${on ? "on" : ""}`}
                    onClick={() =>
                      setAgent({
                        ...agent,
                        tools: on
                          ? agent.tools.filter((x) => x !== toolId)
                          : [...agent.tools, toolId],
                      })
                    }
                  >
                    {toolId}
                  </button>
                );
              })}
            </div>
          )}
          {tab === "limits" && (
            <>
              <div className="field">
                <label>{t("builder.maxSteps")}</label>
                <input
                  type="number"
                  value={agent.limits.max_steps}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      limits: {
                        ...agent.limits,
                        max_steps: Number(e.target.value),
                      },
                    })
                  }
                />
              </div>
              <div className="field">
                <label>{t("builder.maxRuntime")}</label>
                <input
                  type="number"
                  value={agent.limits.max_runtime_seconds}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      limits: {
                        ...agent.limits,
                        max_runtime_seconds: Number(e.target.value),
                      },
                    })
                  }
                />
              </div>
              <div className="field">
                <label>{t("builder.maxRunCost")}</label>
                <input
                  value={agent.budget.max_run_cost ?? ""}
                  onChange={(e) =>
                    setAgent({
                      ...agent,
                      budget: {
                        ...agent.budget,
                        max_run_cost: e.target.value || undefined,
                      },
                    })
                  }
                />
              </div>
            </>
          )}
          {tab === "permissions" && (
            <p style={{ color: "var(--muted)" }}>
              {t("builder.permissionsHint", {
                tools: agent.tools.join(", ") || t("builder.none"),
              })}
            </p>
          )}
          {tab === "lifecycle" && (
            <>
              <p style={{ color: "var(--muted)", marginTop: 0 }}>
                {t("builder.lifecycleFlow")}
              </p>
              <div className="actions" style={{ marginBottom: "1rem" }}>
                <button disabled={busy} onClick={() => control("testing")}>
                  {t("builder.markTesting")}
                </button>
                <button disabled={busy} onClick={() => control("ready")}>
                  {t("builder.markReady")}
                </button>
                <button disabled={busy} onClick={() => control("archive")}>
                  {t("builder.archive")}
                </button>
              </div>
              <label className="check-row">
                <input
                  type="checkbox"
                  checked={overrideTests}
                  onChange={(e) => setOverrideTests(e.target.checked)}
                />
                {t("builder.overrideTests")}
              </label>
              {validation && (
                <div className="validation-list">
                  <h2>
                    {validation.ok
                      ? t("builder.validationOk")
                      : t("builder.validationBlocked")}{" "}
                    · {new Date(validation.checked_at).toLocaleString(i18n.language)}
                  </h2>
                  {validation.checks.map((c) => (
                    <div
                      key={c.id}
                      className={`validation-row ${c.passed ? "pass" : c.blocking ? "fail" : "soft"}`}
                    >
                      <strong>{c.label}</strong>
                      <span>{c.message}</span>
                      <em>
                        {c.passed
                          ? t("builder.pass")
                          : c.blocking
                            ? t("builder.blocking")
                            : t("builder.soft")}
                      </em>
                    </div>
                  ))}
                </div>
              )}
              {!validation && (
                <p style={{ color: "var(--muted)" }}>
                  {t("builder.runValidateHint")}
                </p>
              )}
            </>
          )}
        </div>
      </div>
      <div className="panel">
        <p style={{ margin: 0, color: "var(--muted)" }}>
          <Link to={`/agents/${agent.id}/playground`}>
            {t("builder.footerPlayground")}
          </Link>{" "}
          {t("builder.footerMid")}{" "}
          <Link to={`/agents/${agent.id}/debugger`}>
            {t("builder.footerDebugger")}
          </Link>{" "}
          {t("builder.footerEnd")}
        </p>
      </div>
    </>
  );
}
