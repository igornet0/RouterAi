# Cost management

Pricing is **not** baked into provider adapters. Use `PricingRegistry` / `StaticPricing` / `RemotePricing` / `CustomPricing` and update at runtime:

```rust
client.pricing().upsert(ModelPricing {
    provider: ProviderId::deepseek(),
    model: ModelId::new("deepseek-chat"),
    input_per_million: Some(dec),
    output_per_million: Some(dec),
    cached_input_per_million: Some(dec),
    effective_from: Utc::now(),
});
```

## Actual vs estimated

```rust
let estimate = client.cost().estimate(&request).await?; // output often an upper bound
let response = client.chat()...send().await?;
let actual = response.cost(); // from usage × pricing
```

## Budget policy

```rust
AiClient::builder()
    .budget(BudgetPolicy {
        max_request_cost: Some(Decimal::new(1, 2)),
        max_daily_cost: Some(Decimal::new(1, 0)), // $1/day — useful for BoardDo agents
        max_monthly_cost: None,
    })
```

Pre-flight: estimate → budget check → execute → record actual usage/cost → statistics.

CLI example prices are loaded only when the CLI opts in via `.with_example_prices()` — never treat them as live tariffs.
