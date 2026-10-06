export type AgentStatus =
  | "draft"
  | "testing"
  | "ready"
  | "published"
  | "paused"
  | "archived"
  | "disabled";

export type Agent = {
  id: string;
  lineage_id: string;
  name: string;
  instructions: string;
  model: {
    provider: string;
    model: string;
    temperature?: number;
    max_tokens?: number;
  };
  tools: string[];
  permissions?: { allow?: string[] };
  limits: { max_steps: number; max_runtime_seconds: number };
  budget: { max_run_cost?: string; max_daily_cost?: string };
  status: AgentStatus;
  version: number;
  last_regression_passed?: boolean | null;
  created_at: string;
  updated_at: string;
};

export type Handler = {
  id: string;
  name: string;
  enabled: boolean;
  trigger: { event: string };
  conditions: Array<{
    field: string;
    operator: string;
    value?: unknown;
  }>;
  actions: unknown[];
  max_retries: number;
  timeout_secs: number;
  concurrency: number;
  error_policy: string;
  created_at: string;
  updated_at: string;
};

export type AgentRun = {
  id: string;
  agent_id: string;
  status: string;
  event_id?: string;
  started_at: string;
  finished_at?: string;
  input: unknown;
  output?: unknown;
  error?: unknown;
  usage: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
  cost: string;
  steps: Array<{
    index: number;
    kind: string;
    summary: string;
    detail?: unknown;
    at: string;
    cost?: string;
  }>;
};

export type PlatformEvent = {
  id: string;
  event_type: string;
  source: string;
  timestamp: string;
  payload: unknown;
  metadata?: unknown;
  correlation_id?: string;
  causation_id?: string;
};

export type TestCase = {
  id: string;
  agent_id: string;
  name: string;
  input: unknown;
  expected: unknown[];
  dataset_id?: string;
};

export type EvaluationReport = {
  test_case_id: string;
  run_id: string;
  passed: boolean;
  assertions: Array<{ passed: boolean; message: string }>;
  cost: string;
  latency_ms: number;
};

export type ValidationCheck = {
  id: string;
  label: string;
  passed: boolean;
  blocking: boolean;
  message: string;
};

export type PublishValidationReport = {
  agent_id: string;
  ok: boolean;
  checks: ValidationCheck[];
  checked_at: string;
};

export type PublishResult = {
  agent: Agent | null;
  validation: PublishValidationReport;
  override_used: boolean;
};

export type AuditEntry = {
  id: string;
  action: string;
  actor: string;
  agent_id?: string;
  detail: unknown;
  at: string;
};

export type WebhookTarget = {
  id: string;
  url: string;
  event_types: string[];
  secret?: string;
  enabled: boolean;
};

export type ProviderDescriptor = {
  id: string;
  name: string;
  default_base_url?: string;
  requires_base_url: boolean;
  notes?: string;
};

export type Account = {
  id: string;
  provider: string;
  name: string;
  status: string;
  credit_budget?: string | null;
  created_at: string;
  last_checked_at?: string | null;
};

export type ApiKeyInfo = {
  id: string;
  provider: string;
  account_id: string;
  name?: string | null;
  base_url?: string | null;
  created_at: string;
  last_used_at?: string | null;
  status: string;
};

export type ProviderBalanceReport = {
  provider: string;
  status: string;
  source: string;
  currency?: string;
  total?: string;
  available?: string;
  period_spend?: string;
  tracked_spend?: string;
  credit_budget?: string;
  account_id?: string;
  message?: string;
  updated_at: string;
};

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init?.headers ?? {}),
    },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || res.statusText);
  }
  return res.json() as Promise<T>;
}

export const api = {
  dashboard: () => req<Record<string, unknown>>("/api/v1/dashboard"),
  doctor: () => req<Record<string, unknown>>("/api/v1/doctor"),
  getKill: () => req<{ kill_switch: boolean }>("/api/v1/settings/kill-switch"),
  setKill: (enabled: boolean) =>
    req<{ kill_switch: boolean }>("/api/v1/settings/kill-switch", {
      method: "POST",
      body: JSON.stringify({ enabled, actor: "console" }),
    }),
  listAudit: () => req<{ entries: AuditEntry[] }>("/api/v1/audit"),
  listAgents: (latest = true) =>
    req<{ agents: Agent[] }>(`/api/v1/agents?latest=${latest}`),
  getAgent: (id: string) => req<Agent>(`/api/v1/agents/${id}`),
  createAgent: (agent: Partial<Agent>) =>
    req<Agent>("/api/v1/agents", { method: "POST", body: JSON.stringify(agent) }),
  updateAgent: (id: string, agent: Agent) =>
    req<Agent>(`/api/v1/agents/${id}`, {
      method: "PUT",
      body: JSON.stringify(agent),
    }),
  deleteAgent: (id: string) =>
    req(`/api/v1/agents/${id}`, { method: "DELETE" }),
  validateAgent: (id: string) =>
    req<PublishValidationReport>(`/api/v1/agents/${id}/validate`, {
      method: "POST",
    }),
  publishAgent: (id: string, overrideTests = false) =>
    req<PublishResult>(`/api/v1/agents/${id}/publish`, {
      method: "POST",
      body: JSON.stringify({ override_tests: overrideTests, actor: "console" }),
    }),
  reviseAgent: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/revise`, { method: "POST" }),
  pauseAgent: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/pause`, {
      method: "POST",
      body: JSON.stringify({ actor: "console" }),
    }),
  resumeAgent: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/resume`, {
      method: "POST",
      body: JSON.stringify({ actor: "console" }),
    }),
  archiveAgent: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/archive`, {
      method: "POST",
      body: JSON.stringify({ actor: "console" }),
    }),
  markTesting: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/testing`, {
      method: "POST",
      body: JSON.stringify({ actor: "console" }),
    }),
  markReady: (id: string) =>
    req<Agent>(`/api/v1/agents/${id}/ready`, {
      method: "POST",
      body: JSON.stringify({ actor: "console" }),
    }),
  rollbackAgent: (id: string, toVersion: number) =>
    req<Agent>(`/api/v1/agents/${id}/rollback`, {
      method: "POST",
      body: JSON.stringify({ to_version: toVersion, actor: "console" }),
    }),
  versions: (id: string) =>
    req<{ versions: Agent[] }>(`/api/v1/agents/${id}/versions`),
  runAgent: (id: string, input: unknown) =>
    req<AgentRun>(`/api/v1/agents/${id}/runs`, {
      method: "POST",
      body: JSON.stringify({ input, mode: "interactive" }),
    }),
  playground: (id: string, message: string, context?: unknown) =>
    req<AgentRun>(`/api/v1/agents/${id}/playground`, {
      method: "POST",
      body: JSON.stringify({ message, context }),
    }),
  listRuns: () => req<{ runs: AgentRun[] }>("/api/v1/runs"),
  getRun: (id: string) => req<AgentRun>(`/api/v1/runs/${id}`),
  listEvents: () => req<{ events: PlatformEvent[] }>("/api/v1/events"),
  getEvent: (id: string) => req<PlatformEvent>(`/api/v1/events/${id}`),
  emitEvent: (body: {
    event_type: string;
    source?: string;
    payload?: unknown;
  }) =>
    req<{ event: PlatformEvent; runs: AgentRun[] }>("/api/v1/events", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  listHandlers: () => req<{ handlers: Handler[] }>("/api/v1/handlers"),
  upsertHandler: (h: Handler) =>
    req<Handler>("/api/v1/handlers", { method: "POST", body: JSON.stringify(h) }),
  deleteHandler: (id: string) =>
    req(`/api/v1/handlers/${id}`, { method: "DELETE" }),
  listTools: () =>
    req<{ tools: Array<{ id: string; name: string; description: string }> }>(
      "/api/v1/tools",
    ),
  listSchedules: () => req<{ schedules: unknown[] }>("/api/v1/schedules"),
  upsertSchedule: (s: unknown) =>
    req("/api/v1/schedules", { method: "POST", body: JSON.stringify(s) }),
  tickSchedules: () =>
    req<{ runs: AgentRun[] }>("/api/v1/schedules/tick", { method: "POST" }),
  listCases: (agentId?: string) =>
    req<{ cases: TestCase[] }>(
      agentId ? `/api/v1/test-cases?agent_id=${agentId}` : "/api/v1/test-cases",
    ),
  upsertCase: (c: TestCase) =>
    req<TestCase>("/api/v1/test-cases", {
      method: "POST",
      body: JSON.stringify(c),
    }),
  runCase: (id: string) =>
    req<{ run: AgentRun; evaluation: EvaluationReport }>(
      `/api/v1/test-cases/${id}/run`,
      { method: "POST" },
    ),
  listDatasets: () => req<{ datasets: unknown[] }>("/api/v1/datasets"),
  upsertDataset: (d: unknown) =>
    req("/api/v1/datasets", { method: "POST", body: JSON.stringify(d) }),
  runRegression: (id: string) =>
    req<Record<string, unknown>>(`/api/v1/datasets/${id}/regression`, {
      method: "POST",
    }),
  webhookIngress: (eventType: string, payload: unknown, replyTo?: string) =>
    req<{ event: PlatformEvent; runs: AgentRun[] }>(
      `/api/v1/webhooks/${encodeURIComponent(eventType)}`,
      {
        method: "POST",
        headers: replyTo ? { "X-Reply-To": replyTo } : undefined,
        body: JSON.stringify(payload),
      },
    ),
  listWebhookTargets: () =>
    req<{ targets: WebhookTarget[] }>("/api/v1/webhook-targets"),
  createWebhookTarget: (body: {
    url: string;
    event_types?: string[];
    secret?: string;
  }) =>
    req<WebhookTarget>("/api/v1/webhook-targets", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  deleteWebhookTarget: (id: string) =>
    req(`/api/v1/webhook-targets/${id}`, { method: "DELETE" }),
  listProviders: () =>
    req<{ providers: ProviderDescriptor[] }>("/api/v1/providers"),
  listAccounts: () => req<{ accounts: Account[] }>("/api/v1/accounts"),
  createAccount: (body: { provider: string; name: string }) =>
    req<Account>("/api/v1/accounts", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  setCreditBudget: (accountId: string, credit_budget: string | null) =>
    req<Account>(`/api/v1/accounts/${accountId}/credit-budget`, {
      method: "POST",
      body: JSON.stringify({ credit_budget }),
    }),
  listBalances: () =>
    req<{ balances: ProviderBalanceReport[] }>("/api/v1/balances"),
  listKeys: () => req<{ keys: ApiKeyInfo[] }>("/api/v1/keys"),
  createKey: (body: {
    provider: string;
    secret: string;
    account_id?: string;
    name?: string;
    base_url?: string;
  }) =>
    req<ApiKeyInfo>("/api/v1/keys", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  deleteKey: (id: string) =>
    req<{ ok: boolean }>(`/api/v1/keys/${id}`, { method: "DELETE" }),
};

export function newDraftAgent(name = "New Agent"): Agent {
  const now = new Date().toISOString();
  const id = `agt_draft_${Math.random().toString(16).slice(2, 10)}`;
  return {
    id,
    lineage_id: id,
    name,
    instructions: "You are a helpful agent.",
    model: { provider: "deepseek", model: "deepseek-chat", temperature: 0.2, max_tokens: 1024 },
    tools: ["json.echo", "web.search", "event.emit"],
    permissions: { allow: ["ai", "event_emit"] },
    limits: { max_steps: 20, max_runtime_seconds: 300 },
    budget: { max_run_cost: "0.10" },
    status: "draft",
    version: 1,
    created_at: now,
    updated_at: now,
  };
}

export function agentReplyText(run: AgentRun): string {
  const out = run.output as { text?: string } | undefined;
  if (out?.text) return out.text;
  if (typeof run.output === "string") return run.output;
  if (run.output != null) return JSON.stringify(run.output, null, 2);
  if (run.error != null) return `Error: ${JSON.stringify(run.error)}`;
  return "(no output)";
}

export function statusPillClass(status: string): string {
  if (status === "published") return "ok";
  if (status === "paused" || status === "archived" || status === "disabled") return "bad";
  if (status === "ready" || status === "testing") return "warn";
  return "warn";
}

export function subscribeEvents(onEvent: (e: PlatformEvent) => void): () => void {
  const es = new EventSource("/api/v1/ws/events");
  es.addEventListener("event", (msg) => {
    try {
      onEvent(JSON.parse((msg as MessageEvent).data));
    } catch {
      /* ignore */
    }
  });
  return () => es.close();
}
