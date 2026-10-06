# Observability

## Tracing

| Span / event | Fields |
|---|---|
| span `ai.request` | `logical_request_id`, `model`, `stream` |
| span `ai.attempt` (child) | `attempt_id`, `attempt`, `retry`, `provider`, `key_id` |
| event `attempt settled` (target `universal_ai::accounting`, INFO) | `logical_request_id`, `attempt_id`, `attempt`, `retry`, `provider`, `model`, `key_id`, `latency_ms`, `ttft_ms`, `input_tokens`, `output_tokens`, `cached_tokens`, `reasoning_tokens`, `estimated_cost`, `reserved_cost`, `released_cost`, `charged_cost`, `actual_cost`, `cost_status`, `error_kind` |
| event `logical request succeeded` / `failed` (same target) | `attempts`, `retries`, `fallbacks`, `provider` / `error_kind` |
| event `worst-case estimate` (DEBUG) | the `CostEstimate::explain()` text (sizes and rates only) |
| WARN `request rejected before dispatch`, `retrying on the same provider`, `attempt abandoned before settlement` | ids, reason |
| ERROR `provider billed more output tokens than the reserved bound` | bound violation (should never happen) |

`key_id` is the key record id, never the secret. Prompt and response content are
never logged.

## Telemetry sink

`AiClientBuilder::telemetry(Arc<dyn TelemetrySink>)` (opt-in). Besides
`request_started` / `request_completed` / `request_failed`, every settled
physical attempt calls `attempt_finished(&AttemptReport)` — a serializable record
with the fields above (no content, no secrets).

## Events and statistics

* `client.events().subscribe()` — `RequestStarted` / `RequestCompleted` /
  `RequestFailed` and balance / health events.
* `client.stats().all()` / `today()` — `UsageStatistics` with `charged_cost`
  (equals the ledger) and `unknown_cost_requests`.
* `client.request_usage(attempt_id)`, `client.logical_request_attempts(id)`,
  `client.list_ai_requests(n)` — accounting rows.
