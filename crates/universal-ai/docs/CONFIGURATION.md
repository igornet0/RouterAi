# Configuration

## `AiConfig`

| Field | Default | Meaning |
|---|---|---|
| `default_timeout` | 60 s | deadline per physical attempt (`ChatBuilder::timeout` overrides) |
| `retry_policy` | 3 attempts, 200 ms → 10 s exponential | retries on the same provider ([RETRY_AND_FALLBACK](RETRY_AND_FALLBACK.md)) |
| `fallback` | `false` | try the next eligible provider after a fallbackable error |
| `budget` | no limits | `BudgetPolicy` ([BUDGETS](BUDGETS.md)) |
| `telemetry_enabled` | `false` | set by `AiClientBuilder::telemetry` |
| `store_request_content` | `true` | persist request / response JSON in rows |

`BudgetPolicy { max_request_cost, max_daily_cost, max_monthly_cost, missing_usage }`
(`BudgetPolicy::daily_usd(x)` shortcut). Load from text with
`universal_ai::config::config_from_toml` / `config_from_json` (durations in
seconds / milliseconds).

## `HttpConfig`

| Field | Default |
|---|---|
| `connect_timeout` | 10 s |
| `request_timeout` | 60 s — reqwest total timeout, also bounds whole streams |
| `proxy` | none |
| `default_headers` | none |
| `user_agent` | `universal-ai/<version>` |
| `max_response_bytes` | 32 MiB |

## Builder

```rust,ignore
AiClient::builder()
    .provider(OpenAI::new(key)?)          // or allow_empty_providers() + managed keys
    .config(AiConfig { fallback: true, ..Default::default() })
    .budget(BudgetPolicy::daily_usd(dec!(5)))
    .http_config(HttpConfig::default())
    .storage(Arc::new(SqliteStorage::connect("sqlite://ai.db?mode=rwc").await?))
    .secret_store(store)
    .telemetry(sink)
    .build()?;
```

## Per request (`ChatBuilder`)

| Method | Effect |
|---|---|
| `model`, `message`, `add_message`, `messages`, `tool(s)`, `temperature` | request content |
| `max_tokens` | output bound (reasoning included) |
| `provider` | preferred provider (tried first when eligible) |
| `max_cost` | cap for the logical request (all attempts) — makes it budget-controlled |
| `budget_scope(name, daily_limit)` | also count toward a scope (e.g. `agent:<id>`) |
| `timeout` | per-attempt deadline |
| `cancel_on(future)` | cancel when the future completes (`AiError::Cancelled`, settled `Abandoned`) |
| `request_id(id)` | caller-supplied logical request id |
| `side_effecting(true)` | never fall back |

## Models and pricing at runtime

* `client.pricing().upsert(ModelPricing::per_million(..))` — see [PRICING](PRICING.md).
* `client.models().register(ModelInfo { max_output_tokens, capabilities, .. })` —
  output bound for requests without `max_tokens`, and capability validation
  (streaming, tool calling, output limit) before HTTP.
