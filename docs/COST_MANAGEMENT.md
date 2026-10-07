# Cost management

> Library-level reference: `crates/universal-ai/docs/` — PRICING, BUDGETS,
> STREAMING, RETRY_AND_FALLBACK, STORAGE. This page is the overview.

Pricing is **not** baked into provider adapters. Use `PricingRegistry` / `StaticPricing` / `RemotePricing` / `CustomPricing` and update at runtime:

```rust
let mut price = ModelPricing::per_million(ProviderId::deepseek(), "deepseek-chat", input, output);
price.cached_input_per_million = Some(cache_read);   // else billed at the input rate
price.cache_write_per_million = Some(cache_write);   // else cache writes = unknown cost
price.reasoning_per_million = None;                  // else billed at the output rate
client.pricing().upsert(price);
```

## Budget policy (fail-closed)

```rust
AiClient::builder()
    .budget(BudgetPolicy {
        max_request_cost: Some(Decimal::new(1, 2)),   // worst case per request
        max_daily_cost: Some(Decimal::new(1, 0)),     // UTC day
        max_monthly_cost: None,                       // UTC month
        missing_usage: MissingUsagePolicy::ChargeReserved,
    })
```

Per request: `.max_cost(remaining)` caps one logical request — every attempt's
worst case plus what its earlier retries / fallbacks were charged (agents pass
what is left of `max_run_cost`); `.budget_scope("agent:<id>", Some(daily))` also counts the
spend toward a scope with its own daily limit.

A request is **budget-controlled** when any of these limits applies. Then:

```text
worst-case estimate → reserve (atomic) → provider call → actual usage × pricing → settle
```

- **Unknown cost is never zero.** No price for the model → `AiError::PricingUnavailable`;
  a price sheet missing a rate for a token class actually used, an unpriceable
  usage category (audio, image, …) or inconsistent counts → cost unknown (not $0);
  cached tokens without a cached rate are billed at the input rate, reasoning
  without a reasoning rate at the output rate, cache writes only at their own rate.
- **Worst case** = UTF-8 bytes of the prompt (+ per-message / tool-instruction
  overhead) at the input rate — an upper bound for byte-level tokenizers — plus
  `max_tokens` (or a model limit registered via `client.models().register(..)`) at
  the output rate. Neither known → `AiError::OutputLimitUnknown`. Nothing is sent.
- **Reservation**: the worst case is checked against `max_request_cost` /
  `.max_cost` and against daily / monthly committed spend (settled + in-flight
  reservations) under one lock, then charged immediately and persisted as a
  `Pending` row. Concurrent requests cannot both pass on the same headroom.
- **Settlement** replaces the reservation with the actual cost (unused part is
  released). No usage in the response → the reservation stays charged
  (`CostStatus::UsageUnavailable`); `MissingUsagePolicy::Reject` additionally
  returns `AiError::UsageUnavailable`.
- **Errors**: `PricingUnavailable`, `OutputLimitUnknown`, `BudgetExceeded`,
  `DailyLimitExceeded`, `MonthlyLimitExceeded`, `UsageUnavailable`
  (`AiError::budget_reason()` gives a stable code).

Without any limit, nothing is reserved; unknown costs are recorded as
`cost: None` with `CostStatus::PricingUnavailable` / `UsageUnavailable`, charged
at the worst-case estimate when one can be computed, and counted in
`UsageStatistics::unknown_cost_requests` — not as $0.

## Accounting rows

Each **physical attempt** — first try, every retry, every fallback — is one
`RequestUsage` row with its own reservation and settlement (`accounting.attempt`,
`accounting.retry`, `accounting.logical_request_id` links them;
`client.logical_request_attempts(id)` lists them). `accounting` holds
`estimated_cost`, `reserved_cost`, `charged_cost`, `status`, `budget_scope`,
`dispatched`, `error_kind`, `cost_note`, `rejection`; rejected requests are recorded
too. A definitive HTTP error charges nothing; a timeout, transport error, broken
response or cancellation keeps its reservation (it may have been billed).

Daily / monthly spend is summed from these rows in the client's `Storage`
(SQLite in the server), so it survives restarts:
`client.budget_status(None)` / `client.budget_status(Some("agent:<id>"))`.

## Streaming

Streams pass the same gate. OpenAI-compatible requests send
`stream_options.include_usage`; Anthropic usage comes from `message_start` +
`message_delta`. The stream is settled exactly once when it ends, fails, or is
dropped (dropping closes the connection); without final usage the reservation
stays charged.

CLI example prices are loaded only when the CLI opts in via `.with_example_prices()` — never treat them as live tariffs. `routerai-server` never loads them: its prices come from the operator's price sheet (`pricing.toml`, see the README), and the agent runtime marks every model request `require_cost_bound()`, so an unpriced model is refused before sending instead of costing a silent `$0`.
