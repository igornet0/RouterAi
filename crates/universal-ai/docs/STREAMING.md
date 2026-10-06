# Streaming

`ChatBuilder::stream()` passes the same budget gate as `send()`. The returned
`ChatStream` is **metered** and **pull-based**: no task is spawned, so dropping it
drops the provider stream and closes the HTTP connection immediately.

## Lifecycle

```text
budget check → reservation (Pending row) → connection → chunks → usage → settlement → end
```

Internal phases (`execution::AttemptPhase`): `Reserved → Dispatched → Streaming →
UsageReceived`, ending in exactly one settlement whose persisted outcome is a
`CostStatus`.

| Situation | Result |
|---|---|
| provider ends normally with usage | `Actual`; settled **before** the consumer sees the end |
| usage in the final chunk (OpenAI `include_usage`, Anthropic `message_delta`) | `Actual` |
| usage never arrives | `UsageUnavailable`, reservation charged |
| usage arrives as `null` | ignored (not zero) |
| usage reported more than once | field-wise maximum (never summed, never lowered) |
| malformed chunk | stream ends with `AiError::Serialization`; reservation charged unless final usage arrived |
| provider error event (Anthropic `error`, in-band `{"error":…}`) | stream ends with `AiError::Provider` |
| network disconnect | stream ends with `Network` / `Timeout`; reservation charged unless final usage arrived |
| consumer drops the stream | connection closed; `Abandoned` (actual if final usage arrived, else reservation) |
| process killed | `Pending` row stays charged at the reservation |
| `MissingUsagePolicy::Reject` and no usage | last item is `Err(UsageUnavailable)` |

Usage events emitted by adapters are **final** counts. Settlement methods consume
the meter and `Drop` only settles a meter that was never settled, so double
settlement is impossible (`stream_completed_with_usage_settles_actual_once`
counts storage writes per attempt: exactly reserve + one settlement).

## Timeouts and cancellation

* `ChatBuilder::timeout` / `AiConfig::default_timeout` bound *establishing* the
  stream; a failure there is a normal failed attempt (retry / fallback policy).
* `HttpConfig::request_timeout` (reqwest total timeout, default 60 s) also bounds
  the whole body — raise it for long generations.
* To cancel a stream, drop it.

## Provider support

| Provider | Streaming | Usage source |
|---|---|---|
| OpenAI / DeepSeek / OpenRouter / OpenAI-compatible | yes | final chunk (`stream_options.include_usage` is sent) |
| Anthropic | yes | `message_start` (input) + `message_delta` (output) |
| Gemini | no (rejected before HTTP) | — |
