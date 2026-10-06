import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { AgentRun } from "../api";

export default function DebuggerPanel({ run }: { run: AgentRun }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState<number | null>(null);

  if (!run.steps?.length) return <div className="empty">{t("debugPanel.noSteps")}</div>;

  return (
    <div className="debug-steps">
      {run.steps.map((s) => {
        const isOpen = open === s.index;
        return (
          <div key={s.index} className={`debug-step ${s.kind} ${isOpen ? "open" : ""}`}>
            <button
              className="debug-step-head"
              onClick={() => setOpen(isOpen ? null : s.index)}
            >
              <span className="debug-idx">
                {String(s.index + 1).padStart(2, "0")}
              </span>
              <span className="debug-kind">{s.kind}</span>
              <span className="debug-sum">{s.summary}</span>
              {s.cost != null && <span className="debug-cost">${s.cost}</span>}
            </button>
            {isOpen && (
              <div className="debug-step-body">
                <DetailBlock step={s} run={run} />
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

function DetailBlock({
  step,
  run,
}: {
  step: AgentRun["steps"][number];
  run: AgentRun;
}) {
  const { t, i18n } = useTranslation();
  const detail = step.detail;
  const kind = step.kind.toLowerCase();

  if (kind === "llm" || kind.includes("model")) {
    const d = (detail ?? {}) as {
      model?: string;
      prompt_tokens?: number;
      completion_tokens?: number;
    };
    return (
      <dl className="detail-grid">
        <dt>{t("debugPanel.model")}</dt>
        <dd>{d.model ?? step.summary}</dd>
        <dt>{t("debugPanel.inputTokens")}</dt>
        <dd>{d.prompt_tokens ?? run.usage.prompt_tokens}</dd>
        <dt>{t("debugPanel.outputTokens")}</dt>
        <dd>{d.completion_tokens ?? run.usage.completion_tokens}</dd>
        <dt>{t("debugPanel.cost")}</dt>
        <dd>${step.cost ?? run.cost}</dd>
        <dt>{t("debugPanel.at")}</dt>
        <dd>{new Date(step.at).toLocaleString(i18n.language)}</dd>
        {detail != null && (
          <>
            <dt>{t("debugPanel.detail")}</dt>
            <dd>
              <pre className="pre tight">{JSON.stringify(detail, null, 2)}</pre>
            </dd>
          </>
        )}
      </dl>
    );
  }

  if (kind === "tool" || kind.includes("tool")) {
    const d = (detail ?? {}) as {
      tool?: string;
      input?: unknown;
      output?: unknown;
      duration_ms?: number;
    };
    return (
      <dl className="detail-grid">
        <dt>{t("debugPanel.tool")}</dt>
        <dd>{d.tool ?? step.summary}</dd>
        <dt>{t("debugPanel.duration")}</dt>
        <dd>
          {d.duration_ms != null ? `${d.duration_ms}ms` : t("common.emptyDash")}
        </dd>
        <dt>{t("debugPanel.input")}</dt>
        <dd>
          <pre className="pre tight">
            {JSON.stringify(d.input ?? detail ?? {}, null, 2)}
          </pre>
        </dd>
        <dt>{t("debugPanel.output")}</dt>
        <dd>
          <pre className="pre tight">
            {JSON.stringify(d.output ?? {}, null, 2)}
          </pre>
        </dd>
      </dl>
    );
  }

  return (
    <dl className="detail-grid">
      <dt>{t("debugPanel.kind")}</dt>
      <dd>{step.kind}</dd>
      <dt>{t("debugPanel.summary")}</dt>
      <dd>{step.summary}</dd>
      <dt>{t("debugPanel.at")}</dt>
      <dd>{new Date(step.at).toLocaleString(i18n.language)}</dd>
      {detail != null && (
        <>
          <dt>{t("debugPanel.detail")}</dt>
          <dd>
            <pre className="pre tight">{JSON.stringify(detail, null, 2)}</pre>
          </dd>
        </>
      )}
    </dl>
  );
}
