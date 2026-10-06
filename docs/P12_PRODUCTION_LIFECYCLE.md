# P12 — Production Agent Lifecycle

Prove one full console path without new large subsystems:

```
Draft → Test Lab → Regression → Publish → Handler → Event → AgentRun → Tools → Result → Trace
```

## Lifecycle states

`DRAFT → TESTING → READY → PUBLISHED → PAUSED → ARCHIVED` (+ legacy `DISABLED`)

| Mode | Allowed statuses |
|------|------------------|
| **Interactive** (Playground, Test Lab, manual run) | Draft, Testing, Ready, Published |
| **Automated** (handlers, schedules) | Published only |

## Publish validation

`POST /api/v1/agents/{id}/validate` and publish via control plane check:

- agent configuration (name, instructions)
- model policy present (+ soft: AiClient configured)
- tools exist in registry
- permissions / limits / budget
- secrets (soft placeholder)
- regression / test status (soft — overrideable)
- runtime health / kill switch
- lifecycle status

`POST /api/v1/agents/{id}/publish` body:

```json
{ "override_tests": false, "actor": "console" }
```

If soft test checks fail and `override_tests` is false → `400` policy error.  
If override is true → publish proceeds and **`agent.publish.override_tests` is written to the audit log**.

## Control plane

`ControlPlane` on `RouterRuntime` (not a separate process yet — same crate):

| Action | Route |
|--------|-------|
| Validate | `POST …/validate` |
| Publish | `POST …/publish` |
| Pause / Resume | `POST …/pause` · `…/resume` |
| Archive | `POST …/archive` |
| Mark testing / ready | `POST …/testing` · `…/ready` |
| Rollback version | `POST …/rollback` `{ "to_version": N }` |
| Kill switch | `POST /api/v1/settings/kill-switch` (audited) |
| Audit | `GET /api/v1/audit` |

## Playground + Debugger

- `POST /api/v1/agents/{id}/playground` `{ "message": "…" }` → interactive run
- Console: `/agents/:id/playground` — chat + execution trace (cost, latency, tokens, steps)
- Console: `/agents/:id/debugger` — conversation + clickable step details (LLM / Tool)

## Observability

Runs keep structured steps (`event`, `llm`, `tool`, `observe`, `result`, `error`) with optional `detail` / `cost`. Dashboard and Runs pages remain the operational view.

## Out of scope (later)

- P14 Telegram adapter
- Templates, multi-agent, autonomous loops, vector DB

P13 Webhook: see [P13_WEBHOOK.md](P13_WEBHOOK.md).