# Changelog

## 0.5.0 — P13 Webhook EventSource / EventSink

- `routerai-adapters` crate: webhook source + HTTP sink (no vendor types in core)
- Core `EventSource` / `EventSink` traits + `SinkRegistry`
- Ingress: `POST /api/v1/webhooks/{event_type}` (+ `POST /api/v1/events/{event_type}`)
- Outbound: webhook targets API + `reply_to` / `X-Reply-To` on `agent.completed`
- Console Events page: webhook ingress, curl snippet, sink registration
- Optional `ROUTERAI_WEBHOOK_SECRET` for ingress auth

## 0.4.0 — P12 Production Agent Lifecycle

- Explicit agent statuses: Draft → Testing → Ready → Published → Paused → Archived
- Publish validation (config, model, tools, permissions, budget, secrets, tests, runtime)
- Test override on publish → audit log (`agent.publish.override_tests`)
- Control plane: pause / resume / archive / rollback / kill switch (audited)
- Interactive vs Automated run modes (playground/test vs handlers)
- Playground + Debugger in Web Console; `POST …/playground`, `GET /api/v1/audit`
- Regression updates `last_regression_passed` for publish checks

## 0.3.0 — P11 Web Console + Test Lab

- `web/` Vite/React Console (Dashboard, Agents/Builder, Test Lab, Runs, Events, Handlers, Tools, Costs, Settings)
- API extended: agent CRUD/publish/revise/versions, dashboard, kill switch, test-cases, datasets/regression, event get
- Agent versioning via `lineage_id` + `revise()` → new draft version
- State + Memory stores (stubs, separate from Event/Run)
- Regression aggregates are factual (pass/fail/cost/latency/tool_errors) — no invented quality score
- Server can serve `web/dist`; UI talks **only** REST + SSE

## 0.2.0 — RouterAi Agent Runtime v1

### Separation
- `universal-ai` remains the **model runtime layer**
- `routerai` is the **agent runtime platform** (events / handlers / agents / runs / tools)

### Phase A–E
- Event/Handler/Agent/Run/Tool/Scheduler, AI via AiClient, Test Lab core, REST+SSE, CLI

---

## 0.1.0 — universal-ai initial scaffold

Providers, accounts, pricing, balance, retry/health, SQLite, CLI — see docs.
