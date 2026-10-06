import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Account,
  ApiKeyInfo,
  AuditEntry,
  ProviderBalanceReport,
  ProviderDescriptor,
  api,
} from "../api";

export default function Settings() {
  const { t, i18n } = useTranslation();
  const [kill, setKill] = useState(false);
  const [doctor, setDoctor] = useState<Record<string, unknown> | null>(null);
  const [schedules, setSchedules] = useState<unknown[]>([]);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [everySecs, setEverySecs] = useState(60);
  const [agentId, setAgentId] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [keys, setKeys] = useState<ApiKeyInfo[]>([]);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [balances, setBalances] = useState<ProviderBalanceReport[]>([]);
  const [balanceBusy, setBalanceBusy] = useState(false);
  const [budgetDrafts, setBudgetDrafts] = useState<Record<string, string>>({});
  const [provider, setProvider] = useState("deepseek");
  const [secret, setSecret] = useState("");
  const [keyName, setKeyName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [keyBusy, setKeyBusy] = useState(false);

  const selectedProvider = useMemo(
    () => providers.find((p) => p.id === provider),
    [providers, provider],
  );

  const refresh = () => {
    api.getKill().then((r) => setKill(r.kill_switch));
    api.doctor().then(setDoctor);
    api.listSchedules().then((r) => setSchedules(r.schedules));
    api.listAudit().then((r) => setAudit(r.entries));
    api.listAgents(true).then((r) => {
      if (r.agents[0]) setAgentId(r.agents[0].id);
    });
    api.listProviders().then((r) => setProviders(r.providers));
    api.listKeys().then((r) => setKeys(r.keys));
    api.listAccounts().then((r) => {
      setAccounts(r.accounts);
      const drafts: Record<string, string> = {};
      for (const a of r.accounts) {
        drafts[a.id] = a.credit_budget ?? "";
      }
      setBudgetDrafts(drafts);
    });
  };

  useEffect(() => {
    refresh();
  }, []);

  useEffect(() => {
    if (!selectedProvider) return;
    if (selectedProvider.default_base_url) {
      setBaseUrl(selectedProvider.default_base_url);
    } else if (selectedProvider.requires_base_url) {
      setBaseUrl("");
    }
  }, [selectedProvider]);

  async function toggleKill() {
    const next = !kill;
    await api.setKill(next);
    setKill(next);
    refresh();
  }

  async function addSchedule() {
    if (!agentId) return;
    await api.upsertSchedule({
      id: `sch_${Math.random().toString(16).slice(2, 8)}`,
      name: t("settings.scheduleName"),
      kind: { type: "interval", every_secs: everySecs },
      action: { type: "agent_run", agent_id: agentId },
      enabled: true,
      created_at: new Date().toISOString(),
    });
    setMsg(t("settings.scheduleSaved"));
    refresh();
  }

  async function tick() {
    const r = await api.tickSchedules();
    setMsg(t("settings.tickFired", { count: r.runs.length }));
  }

  async function addKey() {
    setKeyBusy(true);
    setErr(null);
    setMsg(null);
    try {
      if (!secret.trim()) throw new Error(t("settings.keyRequired"));
      if (selectedProvider?.requires_base_url && !baseUrl.trim()) {
        throw new Error(t("settings.baseUrlRequired"));
      }
      await api.createKey({
        provider,
        secret: secret.trim(),
        name: keyName.trim() || undefined,
        base_url:
          selectedProvider?.requires_base_url || provider === "xai"
            ? baseUrl.trim() || undefined
            : baseUrl.trim() && baseUrl !== selectedProvider?.default_base_url
              ? baseUrl.trim()
              : undefined,
      });
      setSecret("");
      setKeyName("");
      setMsg(t("settings.keySaved", { provider }));
      refresh();
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setKeyBusy(false);
    }
  }

  async function removeKey(id: string) {
    setErr(null);
    try {
      await api.deleteKey(id);
      setMsg(t("settings.keyRemoved"));
      refresh();
    } catch (e) {
      setErr(String((e as Error).message || e));
    }
  }

  async function refreshBalances() {
    setBalanceBusy(true);
    setErr(null);
    try {
      const r = await api.listBalances();
      setBalances(r.balances);
      setMsg(t("settings.balancesRefreshed"));
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setBalanceBusy(false);
    }
  }

  async function saveBudget(accountId: string) {
    setErr(null);
    try {
      const raw = (budgetDrafts[accountId] ?? "").trim();
      await api.setCreditBudget(accountId, raw === "" ? null : raw);
      setMsg(t("settings.budgetSaved"));
      refresh();
      await refreshBalances();
    } catch (e) {
      setErr(String((e as Error).message || e));
    }
  }

  function money(v?: string | null) {
    if (v == null || v === "") return t("common.emptyDash");
    return `$${v}`;
  }

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("settings.title")}</h1>
          <p>{t("settings.subtitle")}</p>
        </div>
      </div>
      {msg && <div className="panel">{msg}</div>}
      {err && (
        <div className="panel" style={{ color: "var(--danger, #b33)" }}>
          {err}
        </div>
      )}

      <div className="panel" style={{ marginBottom: "1rem" }}>
        <h2>{t("settings.apiKeys")}</h2>
        <p style={{ color: "var(--muted)" }}>{t("settings.apiKeysHint")}</p>
        <div className="split">
          <div>
            <div className="field">
              <label>{t("common.provider")}</label>
              <select
                value={provider}
                onChange={(e) => setProvider(e.target.value)}
              >
                {(providers.length
                  ? providers
                  : [{ id: "deepseek", name: "DeepSeek" }]
                ).map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>{t("settings.labelOptional")}</label>
              <input
                value={keyName}
                onChange={(e) => setKeyName(e.target.value)}
                placeholder={t("settings.labelPlaceholder")}
              />
            </div>
            <div className="field">
              <label>{t("settings.apiKey")}</label>
              <input
                type="password"
                autoComplete="off"
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
                placeholder="sk-…"
              />
            </div>
            {(selectedProvider?.requires_base_url ||
              provider === "xai" ||
              provider === "openai-compatible") && (
              <div className="field">
                <label>{t("settings.baseUrl")}</label>
                <input
                  value={baseUrl}
                  onChange={(e) => setBaseUrl(e.target.value)}
                  placeholder="https://api.x.ai/v1"
                />
              </div>
            )}
            {selectedProvider?.notes && (
              <p style={{ color: "var(--muted)", fontSize: "0.9rem" }}>
                {selectedProvider.notes}
              </p>
            )}
            <div className="actions">
              <button className="primary" disabled={keyBusy} onClick={addKey}>
                {keyBusy ? t("settings.saving") : t("settings.addKey")}
              </button>
            </div>
          </div>
          <div>
            <table className="table">
              <thead>
                <tr>
                  <th>{t("common.provider")}</th>
                  <th>{t("common.name")}</th>
                  <th>{t("common.status")}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {keys.map((k) => (
                  <tr key={k.id}>
                    <td>
                      <code>{k.provider}</code>
                    </td>
                    <td>{k.name || t("common.emptyDash")}</td>
                    <td>
                      <code>{k.status}</code>
                    </td>
                    <td>
                      <button onClick={() => removeKey(k.id)}>
                        {t("common.remove")}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {keys.length === 0 && (
              <div className="empty">{t("settings.noKeys")}</div>
            )}
          </div>
        </div>
      </div>

      <div className="panel" style={{ marginBottom: "1rem" }}>
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            gap: "1rem",
          }}
        >
          <div>
            <h2>{t("settings.balances")}</h2>
            <p style={{ color: "var(--muted)", margin: 0 }}>
              {t("settings.balancesHint")}
            </p>
          </div>
          <button
            className="primary"
            disabled={balanceBusy}
            onClick={refreshBalances}
          >
            {balanceBusy ? t("settings.checking") : t("settings.checkBalances")}
          </button>
        </div>

        <table className="table" style={{ marginTop: "1rem" }}>
          <thead>
            <tr>
              <th>{t("common.provider")}</th>
              <th>{t("common.status")}</th>
              <th>{t("settings.available")}</th>
              <th>{t("settings.trackedSpend")}</th>
              <th>{t("settings.periodSpend")}</th>
              <th>{t("settings.source")}</th>
            </tr>
          </thead>
          <tbody>
            {balances.map((b) => (
              <tr key={b.provider}>
                <td>
                  <code>{b.provider}</code>
                </td>
                <td>
                  <code>{b.status}</code>
                </td>
                <td>{money(b.available ?? b.total)}</td>
                <td>{money(b.tracked_spend)}</td>
                <td>{money(b.period_spend)}</td>
                <td>
                  <code>{b.source}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {balances.length === 0 && (
          <div className="empty">{t("settings.checkAfterKeys")}</div>
        )}
        {balances.some((b) => b.message) && (
          <ul style={{ color: "var(--muted)", marginTop: "0.75rem" }}>
            {balances
              .filter((b) => b.message)
              .map((b) => (
                <li key={`${b.provider}-msg`}>
                  <strong>{b.provider}:</strong> {b.message}
                </li>
              ))}
          </ul>
        )}

        <h3 style={{ marginTop: "1.25rem" }}>{t("settings.creditBudgets")}</h3>
        <p style={{ color: "var(--muted)" }}>
          {t("settings.creditBudgetsHintBefore")}{" "}
          <a
            href="https://platform.openai.com/settings/organization/billing"
            target="_blank"
            rel="noreferrer"
          >
            {t("settings.openaiBilling")}
          </a>
          {t("settings.creditBudgetsHintAfter")}
        </p>
        <table className="table">
          <thead>
            <tr>
              <th>{t("settings.account")}</th>
              <th>{t("common.provider")}</th>
              <th>{t("settings.creditBudgetUsd")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {accounts.map((a) => (
              <tr key={a.id}>
                <td>{a.name}</td>
                <td>
                  <code>{a.provider}</code>
                </td>
                <td>
                  <input
                    value={budgetDrafts[a.id] ?? ""}
                    onChange={(e) =>
                      setBudgetDrafts({
                        ...budgetDrafts,
                        [a.id]: e.target.value,
                      })
                    }
                    placeholder={t("settings.budgetPlaceholder")}
                  />
                </td>
                <td>
                  <button onClick={() => saveBudget(a.id)}>
                    {t("common.save")}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {accounts.length === 0 && (
          <div className="empty">{t("settings.noAccounts")}</div>
        )}
      </div>

      <div className="split">
        <div className="panel">
          <h2>{t("settings.killSwitch")}</h2>
          <p style={{ color: "var(--muted)" }}>{t("settings.killSwitchHint")}</p>
          <button className={kill ? "primary" : ""} onClick={toggleKill}>
            {kill ? t("settings.disableKill") : t("settings.enableKill")}
          </button>
          <pre className="pre" style={{ marginTop: "1rem" }}>
            {JSON.stringify(doctor, null, 2)}
          </pre>
        </div>
        <div className="panel">
          <h2>{t("settings.schedules")}</h2>
          <div className="field">
            <label>{t("settings.everySeconds")}</label>
            <input
              type="number"
              value={everySecs}
              onChange={(e) => setEverySecs(Number(e.target.value))}
            />
          </div>
          <div className="field">
            <label>{t("settings.agentId")}</label>
            <input value={agentId} onChange={(e) => setAgentId(e.target.value)} />
          </div>
          <div className="actions">
            <button className="primary" onClick={addSchedule}>
              {t("settings.createSchedule")}
            </button>
            <button onClick={tick}>{t("settings.tickNow")}</button>
          </div>
          <pre className="pre">{JSON.stringify(schedules, null, 2)}</pre>
        </div>
      </div>
      <div className="panel" style={{ marginTop: "1rem" }}>
        <h2>{t("settings.auditLog")}</h2>
        <table className="table">
          <thead>
            <tr>
              <th>{t("settings.when")}</th>
              <th>{t("settings.action")}</th>
              <th>{t("settings.actor")}</th>
              <th>{t("common.agent")}</th>
            </tr>
          </thead>
          <tbody>
            {audit.map((e) => (
              <tr key={e.id}>
                <td>{new Date(e.at).toLocaleString(i18n.language)}</td>
                <td>
                  <code>{e.action}</code>
                </td>
                <td>{e.actor}</td>
                <td>
                  <code>{e.agent_id ?? t("common.emptyDash")}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {audit.length === 0 && (
          <div className="empty">{t("settings.noAudit")}</div>
        )}
      </div>
    </>
  );
}
