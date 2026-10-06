# Pricing

Prices live in the `PricingRegistry` (runtime-updatable), never in adapters.
`ModelPricing` has one rate per **token class** (USD per 1M tokens):

```rust,ignore
let mut p = ModelPricing::per_million(ProviderId::anthropic(), "claude-x", input, output);
p.cached_input_per_million = Some(cache_read);
p.cache_write_per_million = Some(cache_write);
p.reasoning_per_million = None; // billed as output
client.pricing().upsert(p);     // negative rates are rejected (try_upsert reports it)
```

## Token classes and rate policy

`Usage` is normalized by each adapter: `prompt_tokens` includes cache reads and
writes, `completion_tokens` includes reasoning. `Usage::breakdown()` splits it into
disjoint classes:

| Class | Tokens | Rate used | Why |
|---|---|---|---|
| `Input` | prompt − cached − cache writes | `input_per_million` | — |
| `CachedInput` | `cached_tokens` | cached rate, else **input rate** | every provider discounts cache reads: can only overestimate |
| `CacheCreation` | `cache_creation_tokens` | `cache_write_per_million` **only** | writes cost more than input (1.25×/2× on Anthropic): input rate would underestimate |
| `Output` | completion − reasoning | `output_per_million` | — |
| `Reasoning` | `reasoning_tokens` | reasoning rate, else **output rate** | OpenAI / Anthropic / Gemini bill reasoning as output |

The cost is **unknown** (`PricingGap` → `CostStatus::PricingUnavailable`) when:

* the model has no price sheet;
* tokens were used in a class whose policy yields no rate;
* `Usage::other_tokens` has a non-zero category (audio, image, 1-hour cache
  writes, server tool requests, an unexplained Gemini total, …) — these are never
  billed at the text rate;
* the counts are inconsistent (cached + writes > prompt, reasoning > completion).

Unknown is never priced as zero; see [BUDGETS](BUDGETS.md) for what is charged.

## Worst-case estimate

```text
input bound  = message bytes + name bytes + tool-call bytes + tool schema bytes
             + response_format bytes + stop bytes + 8 tokens/message + 8
             + 1024 when tools are offered
output bound = max_tokens, else registry max_output_tokens (pinned into the request)
worst case   = input bound  × max(input, cached, cache-write rates that exist)
             + output bound × max(output, reasoning rates)
```

`CostEstimate` carries the itemized `breakdown` (`input_tokens_upper_bound`,
`cached_tokens_upper_bound`, `tool_tokens_upper_bound`,
`output_tokens_upper_bound`, `reasoning_tokens_upper_bound`, rates, assumptions)
and `explain()` renders it — sizes and rates only, never prompt text:

```text
input bytes: 2
message overhead: 16 tokens
input tokens <= 18
max output: 100 (RequestMaxTokens, reasoning included)
input rate: 1000 USD/1M
output rate: 2000 USD/1M
worst-case: 0.218 USD
assumes: 1 input token <= 1 UTF-8 byte (byte-level tokenizers)
assumes: reasoning tokens count toward the output bound (max_tokens)
assumes: no cache-write rate: universal-ai never requests prompt-cache writes; …
```

`client.cost().worst_case(&request)` returns exactly what the budget gate would
reserve (or the error it would reject with); `client.cost().estimate(&request)`
returns an input-only estimate (`output_is_upper_bound == false`) when no output
bound exists.

The estimate is a worst case only under its listed assumptions. Property test
`pricing_properties.rs` checks `cost ≤ estimate` for every usage within the
bounds and every combination of priced classes.

## Multimodal input

Image / audio / file content parts are rejected before dispatch: no adapter
serializes them, so they would be silently dropped and the estimate would not
describe the request actually sent.
