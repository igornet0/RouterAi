# RouterAi Agent Runtime v1 — Technical Spec

Coding-agent contract. Implement by phases; after each phase run `cargo test --workspace`.

## Non-goals (v1)

- Full canvas / n8n clone
- Full macOS Keychain integration beyond universal-ai
- Multi-tenant SaaS auth (stub permissions only)
- Real Telegram adapter (emit events via API/CLI)

## Phase A — Runtime foundation

### Types

```rust
Event { id, event_type, source, timestamp, payload: Value, metadata, correlation_id, causation_id }
Handler { id, name, enabled, trigger, conditions[], actions[], retry_policy, timeout, concurrency, error_policy }
Agent { id, name, instructions, model_policy, tools[], limits, budget, version, status }
AgentRun { id, agent_id, status, started_at, finished_at, input, output, error, usage, cost, trace[] }
ToolDefinition { id, name, description, input_schema, output_schema?, permissions }
Schedule { id, cron_or_interval, action, enabled }
```

### Traits

- `EventBus`: emit, subscribe, list
- `HandlerEngine`: match event → evaluate conditions → enqueue actions
- `AgentRunner`: start/cancel/retry run
- `ToolExecutor`: execute tool by id with sandbox policy
- `Scheduler`: register/tick schedules → emit events or run agents
- `Store`: persist events, handlers, agents, runs, tools, schedules

### SQLite tables

`events`, `handlers`, `agents`, `runs`, `run_steps`, `tools`, `schedules`, `test_cases`, `evaluations`

### Done when

- Emit event → matching handler → creates run record (even if agent is stub)
- Manual + interval schedule tick works
- `cargo test -p routerai` passes

## Phase B — AI execution

- AgentRunner calls `universal_ai::AiClient` for chat steps
- Record usage/cost on run from universal-ai
- Respect agent `budget.max_run_cost` and `limits.max_steps`
- Tool `ai.chat` wraps AiClient

## Phase C — Test Lab

- TestCase { input, expected assertions }
- Run agent against case → Trace → Evaluation report
- Assertions: must_contain, must_not_contain, max_cost, max_latency_ms

## Phase D — API

- `POST /api/v1/events`
- `GET /api/v1/events`
- `CRUD /api/v1/handlers`
- `CRUD /api/v1/agents`
- `POST /api/v1/agents/{id}/runs`
- `GET /api/v1/runs/{id}`
- `POST /api/v1/runs/{id}/cancel`
- `WS /api/v1/ws/events`
- axum + tokio

## Phase E — CLI + hardening

```
routerai doctor
routerai events emit <type> --payload '{}'
routerai handlers list
routerai agents list|run|test
routerai runs list
routerai tools list
```

Security: never log secrets; tool permissions deny-by-default for shell/fs write; kill switch flag on runtime.
