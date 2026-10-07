# Errors

All fallible APIs return `AiResult<T> = Result<T, AiError>`. `AiError` is
`#[non_exhaustive]`; match on `AiError::kind()` for stable classification.

| `ErrorKind` | Variants | Typical cause |
|---|---|---|
| `Budget` | `BudgetExceeded`, `DailyLimitExceeded`, `MonthlyLimitExceeded` | local spend limit |
| `Pricing` | `PricingUnavailable`, `OutputLimitUnknown`, `WorstCaseUnbounded` | cost cannot be bounded (no price, no output bound, or the provider already billed beyond the bound) |
| `Usage` | `UsageUnavailable` | no usage under `MissingUsagePolicy::Reject` |
| `RateLimit` | `RateLimit` | provider 429 (`retry_after_secs()`) |
| `ProviderQuota` | `InsufficientBalance` | provider 402 / exhausted balance |
| `Provider` | `Provider`, `Serialization`, `NoAvailableProvider` | 5xx, malformed response, no provider left |
| `Network` | `Network` | transport failure |
| `Timeout` | `Timeout` | attempt deadline / HTTP timeout / 408 |
| `Cancellation` | `Cancelled` | `ChatBuilder::cancel_on` fired |
| `Authentication` | `Authentication`, `Authorization` | 401 / 403, no key bound |
| `Validation` | `InvalidRequest`, `UnsupportedModel`, `UnsupportedCapability` | bad request, unsupported content / capability (detected before HTTP when possible) |
| `Configuration` | `Config` | invalid configuration |
| `Storage` | `Storage`, `SecretStore` | persistence / secret store failure |
| `NotFound` | `NotFound` | unknown id |
| `Internal` | `Unknown` | unexpected |

Helpers:

* `is_retryable()` — retrying the same provider may help.
* `is_fallbackable()` — another provider may help.
* `may_have_consumed_tokens()` — financial uncertainty (reservation kept).
* `budget_reason()` — stable snake_case code for budget / pricing refusals.
* `retry_after_secs()` — provider hint (`Retry-After` header).

Messages never contain secrets: HTTP bodies pass through `sanitize_message`
(all occurrences of key-like patterns) and adapters redact the exact credential
value they sent (`AiError::redact_secret`). Error kinds are also persisted per
attempt (`CostAccounting::error_kind`).
