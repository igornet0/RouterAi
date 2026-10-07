# RouterAi

**Agent Runtime Platform** on top of **universal-ai** (model runtime).

```text
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

### Supported toolchains

| Toolchain | Status | Checked by |
|---|---|---|
| Rust **1.88** (MSRV, `rust-version`) | supported | CI `msrv`: `cargo test --locked` on Linux |
| Rust **1.98.0** (pinned, known good) | supported | CI `lint` (fmt, clippy `-D warnings`) and `test` on Linux + macOS |
| latest stable | expected to work | not gated (new clippy lints may appear) |

The MSRV is set by the locked dependencies (`icu_*`, `home` require 1.88), not by
RouterAi's own code; older toolchains cannot even parse their manifests. It is
proven against the committed `Cargo.lock` — every CI cargo command runs with
`--locked`, so a dependency update that raises it fails the `msrv` job.
Building `routerai-server` needs `web/dist` (Node 22: `cd web && npm ci && npm run build`).

### Server configuration: prices and secrets

| Setting | Default | Meaning |
|---|---|---|
| `ROUTERAI_DATA_DIR` / `--data-dir` | `~/.routerai` | databases, key metadata, encrypted secrets |
| `ROUTERAI_PRICING_FILE` / `--pricing-file` | `<data dir>/pricing.toml` | price sheet (see below); a model without an entry is refused before any request (`PricingUnavailable`) |
| `ROUTERAI_SECRETS_KEY` | — | 64 hex chars: AES-256-GCM key of `<data dir>/secrets.enc` |
| `ROUTERAI_SECRETS_KEY_FILE` | `<data dir>/secrets.key` (created, `0600`, with a warning) | key file used when `ROUTERAI_SECRETS_KEY` is unset; keep it **outside** the data directory in production |

The server never loads built-in or demo prices. Every rate comes from the price
sheet with its source and verification date (`GET /api/v1/pricing` lists them):

```toml
[[price]]
provider = "openai"
model = "gpt-4o-mini"
input_per_million = "0.15"          # USD, strings (exact decimals)
output_per_million = "0.60"
cached_input_per_million = "0.075"  # optional; also cache_write_ / reasoning_per_million
source = "https://openai.com/api/pricing"
as_of = "2026-10-01"                # last verified; entries older than 90 days are logged
```

Unknown keys, float or negative rates, missing `source` / `as_of`, future dates
and duplicate entries stop the server at startup. A self-hosted model is free
only if its entry says `"0"`. Agent requests are always cost-bounded: without a
price — or without `max_tokens` / a known model output limit — they are refused
before sending, and an attempt whose cost cannot be determined is charged its
reservation, never `$0`.

Secrets are stored in `secrets.enc` (AES-256-GCM, owner-only). A legacy
`secrets.bin` (XOR-obfuscated, written by older versions) is imported and deleted
on startup — rotate those keys if the old file may have been copied.

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

## Agents with tools

The agent loop is: model turn → tool calls → policy check → tool execution →
tool results back to the model → next turn, until the model answers without
tool calls. Limits: `limits.max_steps` (model turns per run) and
`limits.max_runtime_seconds` (whole run, including each model request).

Tools are offered **only** from the agent allow-list (`agent.tools`; empty = no
tools), filtered by `agent.permissions` and the runtime deny list. Run input
(webhooks, events) can never invoke tools directly. Tool failures, unknown
tools and malformed arguments go back to the model as error tool results.

```rust,no_run
use std::sync::Arc;

use routerai::*;
use serde_json::{json, Value};
use universal_ai::{AiClient, OpenAI};

struct PriceLookup;

#[async_trait::async_trait]
impl ToolHandler for PriceLookup {
    async fn call(&self, input: Value) -> RouterResult<Value> {
        let sku = input["sku"].as_str().unwrap_or_default();
        Ok(json!({ "sku": sku, "price_usd": 42 }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ai = AiClient::builder()
        .provider(OpenAI::new(std::env::var("OPENAI_API_KEY")?)?)
        .build()?;
    let rt = RouterRuntime::builder().ai(Arc::new(ai)).build().await?;

    rt.tools()
        .registry()
        .register(
            ToolDefinition {
                id: "shop.price".into(),
                name: "Price lookup".into(),
                description: "Current price for a SKU".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "sku": { "type": "string" } },
                    "required": ["sku"]
                }),
                output_schema: None,
                permissions: Permissions::none(),
            },
            Arc::new(PriceLookup),
        )
        .await?;

    let mut agent = published_agent("sales", "Answer price questions using tools.");
    agent.model.provider = "openai".into();
    agent.model.model = "gpt-4o-mini".into();
    agent.tools = vec!["shop.price".into()];
    agent.limits.max_steps = 6;
    let agent_id = agent.id.clone();
    rt.upsert_agent(agent).await?;

    let run = rt
        .start_run(&agent_id, json!({ "message": "How much is SKU-7?" }), None)
        .await?;
    println!("{:?}: {}", run.status, run.output.unwrap_or_default()["text"]);
    Ok(())
}
```

Without the runtime, `universal-ai` exposes the same flow on `ChatBuilder`:

```rust,no_run
use serde_json::json;
use universal_ai::{AiClient, Message, OpenAI, Tool, ToolResult};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let ai = AiClient::builder().provider(OpenAI::new("sk-...")?).build()?;
let tools = vec![Tool::function(
    "get_weather",
    "Current weather for a city",
    json!({ "type": "object", "properties": { "city": { "type": "string" } } }),
)];

let question = Message::user("Weather in Paris?");
let first = ai
    .chat()
    .model("gpt-4o-mini")
    .add_message(question.clone())
    .tools(tools.clone())
    .send()
    .await?;

let mut history = vec![question, first.message.clone()];
for call in first.tool_calls() {
    let args = call.arguments_json()?; // {"city": "Paris"}
    let _ = args;
    history.push(ToolResult::success(call.id.clone(), r#"{"temp_c":21}"#).into());
}
let answer = ai
    .chat()
    .model("gpt-4o-mini")
    .messages(history)
    .tools(tools)
    .send()
    .await?;
println!("{}", answer.text());
# Ok(())
# }
```

Streaming (`.stream()`) does not assemble tool calls yet; agents use the
non-streaming loop.

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
