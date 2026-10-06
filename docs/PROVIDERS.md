# Providers

| Provider | Wire format | Notable capabilities |
|----------|-------------|----------------------|
| `OpenAI` | OpenAI chat/completions | chat, stream, tools, images, moderation |
| `DeepSeek` | OpenAI-compatible + `/user/balance` | chat, stream, **balance** |
| `Anthropic` | Messages API | chat, stream, tools, prompt cache fields |
| `Gemini` | Generative Language `generateContent` | chat, model list, embeddings flag |
| `OpenRouter` | OpenAI-compatible | multi-model gateway |
| `OpenAICompatible` | OpenAI-compatible | local LLMs, gateways, custom base URL |

## Capability checks

```rust
if provider.supports(Capability::Balance) {
    let balance = provider.balance().await?;
}
```

If a provider has no balance API, adapters return `Ok(None)` or `UnsupportedCapability` — they **never invent** a balance.

## Escape hatch

`ChatResponse.raw` retains provider JSON when available. Dedicated adapters expose provider-specific endpoints (e.g. DeepSeek balance) without forcing them into the core chat model.

## Local models

Point `OpenAICompatible` at:

- Ollama (`http://127.0.0.1:11434/v1`)
- vLLM / llama.cpp server / LM Studio OpenAI endpoints
