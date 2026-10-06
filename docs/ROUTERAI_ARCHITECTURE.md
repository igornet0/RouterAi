# RouterAi Architecture

## Separation of concerns

| Layer | Responsibility |
|-------|----------------|
| **universal-ai** | Model runtime: providers, accounts, keys, pricing, budget, routing, health, usage |
| **RouterAi** | Agent runtime: events, handlers, agents, runs, tools, scheduler, eval, API |

**AI models are not the agent runtime.** Agents call models through `universal_ai::AiClient`.

## Lifecycle

```
CREATE → CONFIGURE → VALIDATE → TEST → PUBLISH → RUN → OBSERVE → EVALUATE → VERSION → ROLLBACK
```

## Core flow (not n8n)

```
Event → Event Router → Handler → Agent / Workflow / Tool → Result → Event
```

## Layers

```
UI (Agents / Events / Tools / Runs / Costs)
        │ REST / WebSocket
API (Agent / Event / Run / Tool)
        │
Agent Runtime (Trigger → Context → Loop → Actions → Memory/Tools/State)
        │
Event Runtime (Bus / Scheduler / Handlers / Queues / Retry / DLQ)
        │
universal-ai (Providers / Pricing / Budget / Router / Health)
```

## Workspace (v1)

Consolidated crates first (split later when boundaries harden):

| Crate | Role |
|-------|------|
| `universal-ai` | Existing model layer |
| `routerai` | Core + runtime + events + agents + tools + eval + storage |
| `routerai-adapters` | Channel adapters (webhook P13; telegram P14) |
| `routerai-server` | REST + WebSocket API |
| `routerai-cli` | `routerai` binary |

## MVP phases

- **A** — Event, Handler, Agent, Run, Tool, Scheduler, SQLite
- **B** — Agent execution via universal-ai (usage/cost)
- **C** — Test Lab (cases, assertions, traces)
- **D** — REST/WebSocket API
- **E** — CLI, permissions skeleton, doctor, tests

See `docs/ROUTERAI_TZ.md` for the coding-agent implementation contract.
