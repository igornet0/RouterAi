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

## Tiered (long-context) prices

The rates of `ModelPricing` are the **base tier**. A model whose price depends on
the prompt size adds explicit `Tiering` — a sheet without it is a flat price,
nothing else is assumed:

```rust,ignore
let mut p = ModelPricing::per_million(ProviderId::gemini(), "gemini-x", base_in, base_out);
p.tiering = Some(Tiering {
    mode: TierMode::WholeRequest, // required: there is no default mode
    tiers: vec![PriceTier {
        above_input_tokens: 200_000,
        input_per_million: in_above,
        output_per_million: out_above,
        cached_input_per_million: None, // exactly the optional rates the base sets
        cache_write_per_million: None,
        reasoning_per_million: None,
    }],
});
client.pricing().try_upsert(p)?;
```

**Measure.** The tier is chosen by the request's input tokens,
`Usage::prompt_tokens` — cache reads and writes included. A tier applies when
input tokens are **strictly greater** than its threshold: with 200 000, a
200 000-token prompt is billed at the lower tier, 200 001 at the higher one.

**Modes** (`TierMode`):

| Mode | Input-side tokens | Output / reasoning |
|---|---|---|
| `WholeRequest` | every token at the rates of the tier the prompt falls into ("0–200k → A, > 200k → B") | that tier's rates |
| `Marginal` | per band: the first 200k at A, the rest at B; inside the prompt, cache reads come first, then cache writes, then uncached input | the rates of the tier the whole prompt reaches |

`Marginal` is RouterAi's definition, not a provider's: check that it matches how
the provider actually bills before choosing it.

**Validation** (`validate`, `try_upsert`; the server's `pricing.toml` uses the
same rules): thresholds positive and strictly increasing; a tiered sheet has base
input and output rates; each tier states exactly the optional rates (cached
input, cache write, reasoning) the base states — a tier never inherits a rate;
no negative rate in any tier.

**Worst case.** The tiers *reachable* by the input bound are the base and every
tier whose threshold is below the bound. The reservation uses the highest
input-side and output-side rates over all reachable tiers, so it covers both
modes — and a cheaper upper tier never lowers it. `explain()` says which tiers
were reachable. The property test runs half of its cases on random tiered sheets
(1–3 tiers, both modes, rates not necessarily increasing).

`Cost::tier_above_input_tokens` records the highest tier a settled request
reached (`None` = base / flat).

## Price versions (history)

A price sheet's `version()` is a content hash (`pv1-…`): identical sheets share
it, any change — a rate, a tier, the mode, `effective_from` — gives a new one.

* The budget gate looks the price up **once** per attempt; the estimate, the
  reservation and the settlement all use that sheet, even if the registry is
  updated while the attempt is in flight.
* The attempt row records `CostAccounting::pricing_version`, and the sheet itself
  is saved by the storage (`Storage::save_pricing_version`, SQLite table
  `price_versions`) **before** the first row that references it.
* `reconcile_attempt` with `Reconciliation::Usage` reprices with the attempt's
  own sheet — from the registry's history, else from storage after a restart —
  never with today's price. A stored sheet that no longer hashes to its version
  is refused (`corrupt price version`). Rows from before price versions existed
  are repriced at the current sheet; the audit note says so, and the row then
  records that version.
* `PricingRegistry::get_version` returns any sheet the registry has held.

## Multimodal input

Image / audio / file content parts are rejected before dispatch: no adapter
serializes them, so they would be silently dropped and the estimate would not
describe the request actually sent.
