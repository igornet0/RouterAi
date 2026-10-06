# Adding a provider

1. Create a type in `src/providers/your_provider.rs`.
2. Implement `Provider` (only override what you support).
3. Advertise accurate `ProviderCapabilities`.
4. Export from `providers/mod.rs`.
5. **Do not** change `AiClient` core API.
6. **Do not** hardcode live prices in the adapter — register them via `PricingRegistry`.

```rust
use async_trait::async_trait;
use universal_ai::*;

pub struct MyProvider { /* http, secrets, base_url */ }

#[async_trait]
impl Provider for MyProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("my-provider")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: true,
            ..Default::default()
        }
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        // map canonical ChatRequest → wire → ChatResponse
        todo!()
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        Ok(HealthStatus::ok(0))
    }
}
```

Prefer composing `OpenAICompatible` when the upstream speaks OpenAI chat/completions.

For unique features (balance, native tools, etc.), add methods on the concrete type or use `ChatResponse.raw`.
