# Retry and fallback

| Term | Meaning |
|---|---|
| logical request | one `send()` / `stream()` call (`logical_request_id`) |
| physical attempt | one HTTP request to a provider: own row, reservation, settlement |
| retry | a new physical attempt on the **same** provider (same bound key) |
| fallback | a new physical attempt on the **next** eligible provider |

Skipping a provider that cannot serve the request (missing capability, model
limit, unhealthy) is not a fallback: nothing is attempted on it.

## Policy

`RetryPolicy { max_attempts (per provider, incl. first try), initial_delay,
max_delay, exponential_backoff }`; fallback requires `AiConfig::fallback = true`
and a request not marked `side_effecting`.

| Error | Retry same provider | Fallback | Financially | Reservation |
|---|---|---|---|---|
| timeout (deadline, reqwest, HTTP 408) | yes | yes | uncertain | kept |
| connection reset / transport error | if transient | yes | uncertain | kept |
| HTTP 429 | yes (honours `Retry-After`; longer than `max_delay` → no retry) | yes | not charged | released |
| HTTP 500 / 503 | yes | yes | not charged | released |
| HTTP 502 / 504 / 524 | yes | yes | uncertain (gateway) | kept |
| HTTP 501 / 505 | no | yes | not charged | released |
| malformed response | no | yes | uncertain | kept |
| authentication / authorization (401 / 403) | no | yes | not charged | released |
| provider quota (402) | no | yes | not charged | released |
| invalid request (400 / 422) | no | no | not charged | released |
| missing key / secret for a provider | no | yes | not sent | — |
| `PricingUnavailable`, `OutputLimitUnknown` | no | no | not sent | — |
| `BudgetExceeded`, `DailyLimitExceeded`, `MonthlyLimitExceeded` | no | no | not sent | — |
| `UsageUnavailable` (Reject policy) | no | no | charged | kept |
| `Cancelled` | no | no | uncertain | kept (0 if not dispatched) |

*Uncertain* = the provider may have generated (and billed) tokens; the
reservation stays charged. *Released* = the attempt is settled at 0.

## No double charge, no hidden charge

* Every retry / fallback passes the budget gate again with its own reservation:
  when the budget is exhausted, no further HTTP attempt happens
  (`budget_exhaustion_stops_further_attempts`).
* Per-request caps (`max_cost`, `max_request_cost`) cover the logical request:
  earlier attempts' charges + the next worst case
  (`max_cost_caps_the_logical_request_across_retries`).
* A successful attempt is charged its actual cost once; failed attempts are
  charged only per the table above (`retries_are_separate_attempts_with_own_reservation`,
  `retry_after_timeout_keeps_the_ambiguous_charge`,
  `fallback_after_malformed_response_keeps_both_charges`).
* The returned `ChatResponse::request_id` is the answering attempt; use
  `AiClient::logical_request_attempts` for all of them.

`retry::with_retry` is a generic helper. `AiClient` does not use it; wrapping an
adapter's `chat` with it would bypass accounting.
