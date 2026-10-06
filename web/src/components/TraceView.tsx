import { useTranslation } from "react-i18next";
import type { AgentRun } from "../api";

export default function TraceView({ steps }: { steps: AgentRun["steps"] }) {
  const { t } = useTranslation();
  if (!steps?.length) return <div className="empty">{t("debugPanel.noSteps")}</div>;
  return (
    <div className="trace">
      {steps.map((s) => (
        <div key={s.index} className={`step ${s.kind}`}>
          <strong>{s.kind}</strong> — {s.summary}
          {s.cost != null && <span> · ${s.cost}</span>}
        </div>
      ))}
    </div>
  );
}
