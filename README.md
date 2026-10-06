# RouterAi

**Agent Runtime Platform** on top of **universal-ai** (model runtime).

```
Events → Handlers → Agents → Tools → Results → Events
                         ↓
                   universal-ai
              (providers / pricing / budget)
```

AI models are **not** the agent runtime. Agents call models only through `universal_ai::AiClient`.

## Workspace

| Crate / app | Role |
|-------------|------|
| `universal-ai` | Providers, accounts, keys, pricing, budget, routing |
| `universal-ai-cli` | `ai` CLI for model layer |
| `routerai` | Event bus, handlers, agents, runs, tools, scheduler, test lab |
| `routerai-adapters` | Webhook (P13); Telegram later (P14) |
| `routerai-server` | REST + SSE API (+ optional static Console) |
| `routerai-cli` | `routerai` CLI |
| `web/` | RouterAi Web Console (Vite/React) |

```bash
cargo test --workspace
cargo run -p routerai-cli -- doctor

# API + embedded Web Console (rebuild web/dist is pulled in at compile time)
cargo run -p routerai-server -- --port 8787
# → http://127.0.0.1:8787/

# Optional: --host 0.0.0.0 · --bind 127.0.0.1:9000 · --web-dir ./web/dist

cd web && npm install && npm run build   # refresh Console before cargo rebuild
cd web && npm run dev                    # hot reload at :5173 (proxies API)
```

## Web Console (P11–P13)

Proves: **create → test → publish → webhook → run → sink → trace**.

- Agent Builder + lifecycle (validate / publish / pause / resume)
- Playground (chat + execution trace) and Debugger (step details)
- Test Lab + regression (factual metrics; feeds publish checks)
- Webhook EventSource/Sink (`curl` ingress + callback targets)
- Runs + live SSE, audit log, kill switch

See [docs/P11_WEB_CONSOLE.md](docs/P11_WEB_CONSOLE.md), [docs/P12_PRODUCTION_LIFECYCLE.md](docs/P12_PRODUCTION_LIFECYCLE.md), [docs/P13_WEBHOOK.md](docs/P13_WEBHOOK.md).

## Quick lifecycle (runtime)

```rust
use routerai::*;
use serde_json::json;

#[tokio::main]
async fn main() -> RouterResult<()> {
    let rt = RouterRuntime::builder().build().await?;

    let agent = published_agent("sales", "You are a sales agent.");
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await?;

    rt.upsert_handler(Handler::agent_on_event(
        "Sales handler",
        "telegram.message.received",
        agent_id,
    )).await?;

    let runs = rt.emit(Event::new(
        "telegram.message.received",
        "telegram",
        json!({"chat_id": 123, "text": "Хочу купить продукт"}),
    )).await?;

    println!("{:?}", runs[0].status);
    Ok(())
}
```

## Docs

- [docs/ROUTERAI_ARCHITECTURE.md](docs/ROUTERAI_ARCHITECTURE.md)
- [docs/ROUTERAI_TZ.md](docs/ROUTERAI_TZ.md)
- [docs/P11_WEB_CONSOLE.md](docs/P11_WEB_CONSOLE.md)
- [docs/P12_PRODUCTION_LIFECYCLE.md](docs/P12_PRODUCTION_LIFECYCLE.md)
- [docs/P13_WEBHOOK.md](docs/P13_WEBHOOK.md)
- [ARCHITECTURE.md](ARCHITECTURE.md) — universal-ai
- [SECURITY.md](SECURITY.md)

## Not an n8n clone

RouterAi is an **event → agent** runtime with tools, budgets, and evaluation — not a node canvas workflow engine.

## License

MIT OR Apache-2.0
