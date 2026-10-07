# Providers

| Adapter | Chat | Stream | Tools | Structured output | Balance | Usage normalization |
|---|---|---|---|---|---|---|
| `OpenAI` | yes | yes | yes | yes | — (costs API with admin key) | `prompt_tokens_details.cached_tokens` → cached; `completion_tokens_details.reasoning_tokens` → reasoning; other details (`audio_tokens`, `image_tokens`, unknown) → `other_tokens` (unpriceable) |
| `DeepSeek` | yes | yes | yes | yes | yes | as OpenAI; `prompt_cache_hit_tokens` → cached |
| `OpenRouter` | yes | yes | yes | yes | — | as OpenAI |
| `OpenAICompatible` | per `capabilities()` | | | | | as OpenAI |
| `Anthropic` | yes | yes | yes | no (not mapped) | — | prompt = input + cache reads + cache writes; cache writes → `cache_creation_tokens`; 1-hour cache writes / server tool requests → `other_tokens`; output includes thinking |
| `Gemini` | yes | no | yes | no (not mapped) | — | completion = candidates + `thoughtsTokenCount` (reasoning); prompt += `toolUsePromptTokenCount`; `cachedContentTokenCount` → cached; non-text modalities and unexplained `totalTokenCount` → `other_tokens` |

## Output limit

`ChatRequest::max_tokens` (`ChatBuilder::max_tokens`) bounds **all** output tokens,
reasoning / thinking included; it is the output bound of the worst case. Each
adapter sends it — for chat and streams alike — in the parameter with that
meaning:

| Adapter | Wire parameter | Reasoning inside the bound |
|---|---|---|
| `OpenAI` | `max_completion_tokens` | yes (documented by OpenAI; reasoning models reject `max_tokens`) |
| `DeepSeek`, `OpenRouter`, `OpenAICompatible` | `max_tokens` (`OpenAICompatibleBuilder::output_limit_param` switches to `max_completion_tokens`) | provider-dependent — not proven |
| `Anthropic` | `max_tokens` (required by the API: 1024 is sent when an *uncontrolled* request has none) | yes (thinking counts toward `max_tokens`; the adapter does not enable thinking) |
| `Gemini` | `generationConfig.maxOutputTokens` | not proven |

Budget-controlled requests always carry a bound: the request's `max_tokens`, or
the registry's `max_output_tokens` pinned into the request; with neither they are
refused before HTTP (`OutputLimitUnknown`). Where reasoning inside the bound is
not proven, a provider that bills beyond it trips `WorstCaseUnbounded` (see
[BUDGETS](BUDGETS.md#when-the-provider-bills-beyond-the-worst-case)). Proven by
`tests/output_limits.rs` (every adapter, chat + stream, explicit and pinned
bounds, refusal without a bound).

Capabilities are checked before any reservation or HTTP: a request that needs
streaming, tools or structured output skips providers without them; if none is
left the request fails with `UnsupportedCapability`.

## Layers inside an adapter

| Concern | Where |
|---|---|
| transport | shared `HttpClient` (timeouts, proxy, max body, typed transport errors) |
| protocol | adapter `to_wire` / `parse_*` (OpenAI wire, Anthropic Messages, Gemini generateContent) |
| usage extraction | adapter `*_usage` → canonical `Usage` |
| pricing | **not** in adapters — `PricingRegistry` |
| capabilities | `Provider::capabilities()` + model registry |
| authentication | `Provider::with_credential` binds one key per attempt |

## Adding a provider

Implement `Provider` (`id`, `capabilities`, `chat`, optionally `stream_chat`,
`health`, `with_credential`) and follow these rules:

1. Map usage to the canonical semantics (prompt includes cache classes,
   completion includes reasoning); put anything else in `other_tokens`.
   Return `usage: None` when the provider sent none — never zero counts.
2. Streams: build with `sse_stream`-style pull-based streams (no spawned task);
   emit `StreamEvent::Usage` only with final counts; end the stream with an error
   on malformed or error events.
3. Errors: use the shared HTTP helpers (`error_from_response`,
   `transport_error`) and `redact_secret` with the credential you sent.
4. Advertise only capabilities the adapter actually maps.

See also the repository-level [ADDING_PROVIDER](../../../docs/ADDING_PROVIDER.md).
