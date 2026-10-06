# P11 — RouterAi Web Console

Web UI proves the full loop via **REST + SSE only**. No agent logic in the frontend. No Telegram binding. No n8n canvas.

## Run

```bash
# terminal 1 — API (+ serves web/dist if built)
cargo run -p routerai-server

# terminal 2 — UI (dev with proxy)
cd web && npm install && npm run dev
# → http://127.0.0.1:5173

# or production static:
cd web && npm run build
cargo run -p routerai-server
# → http://127.0.0.1:8080
```

## Sections

Dashboard · Agents / Agent Builder · Test Lab · Runs · Events · Handlers · Tools · Costs · Settings

## Acceptance (P11)

- Create / edit / delete agent
- Versioning (`publish`, `revise` → new draft version, same lineage)
- Model / tools / budget / permissions via builder
- Handlers + schedules
- Emit event from UI
- Run agent + trace
- Live SSE on Runs / Events
- Test Lab + cases + assertions + regression aggregates (factual)
- Run history, event inspector, costs, kill switch, errors
- API/CLI unchanged in role; `cargo test --workspace` PASS
