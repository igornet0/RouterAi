# Account management

Accounts and API keys are separate entities:

```
Account
 ├── API Key #1 (metadata + SecretStore entry)
 ├── API Key #2
 └── API Key #3
```

## Accounts

```rust
let account = client.accounts()
    .add_account(ProviderId::deepseek(), "production")
    .await?;
client.accounts().check(&account.id).await?;
```

## Keys

```rust
let info = client.keys().add_key(AddKeyRequest {
    provider: ProviderId::deepseek(),
    account_id: account.id,
    secret: SecretString::from(std::env::var("DEEPSEEK_API_KEY")?),
    name: Some("primary".into()),
    base_url: None, // or the endpoint this key belongs to
}).await?;
client.sync_provider_from_key(&info, info.base_url.as_deref()).await?;

// Rotation = new secret + deprecate old (manual replacement when provider has no key API)
client.keys().rotate_key(&old_id, new_secret).await?;
```

Metadata (`ApiKeyInfo`) never contains the secret. Multi-key strategies: `FirstAvailable`, `RoundRobin`, `LeastUsed`, `LowestCost`, `HighestBalance`.

## How a key reaches the provider

```text
request → select_key(provider) → secret from SecretStore
        → provider.with_credential(key) → HTTP call → RequestUsage.api_key = that key
```

- Providers registered for managed keys (`sync_provider_from_key`) hold **no
  secret**. For every provider attempt `AiClient` selects a key and binds it to a
  fresh adapter instance (`Provider::with_credential`); that instance is the only
  place the secret lives and is dropped after the request.
- Retries on the same provider (each its own physical attempt with its own
  accounting row) reuse the same bound key; round-robin advances once per
  provider per logical request, with a separate cursor per provider.
- Disabled / deprecated / deleted keys are never selected. Deleting a key
  removes it from selection first, then from the `SecretStore`; a request that was
  already in flight may finish with it, nothing started afterwards can use it. When
  the last key is gone, requests fail with an authentication error.
- A key's `base_url` overrides the provider endpoint, so a secret is only sent to
  the endpoint it was registered for.
- Providers built with a key in their constructor (`OpenAI::new(key)`) keep using
  it only while no managed keys exist for that provider id; `RequestUsage.api_key`
  is then `None`.
- Per-key `ApiKeyInfo.usage` is updated from the request that used the key.


## Isolation

Each recorded `RequestUsage` carries `provider`, `account`, `api_key` (id), `model`, `request_id` for SaaS-style tenancy.
