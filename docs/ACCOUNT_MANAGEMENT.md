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
client.keys().add_key(AddKeyRequest {
    provider: ProviderId::deepseek(),
    account_id: account.id,
    secret: SecretString::from(std::env::var("DEEPSEEK_API_KEY")?),
    name: Some("primary".into()),
}).await?;

// Rotation = new secret + deprecate old (manual replacement when provider has no key API)
client.keys().rotate_key(&old_id, new_secret).await?;
```

Metadata (`ApiKeyInfo`) never contains the secret. Multi-key strategies: `FirstAvailable`, `RoundRobin`, `LeastUsed`, `LowestCost`, `HighestBalance`.

## Isolation

Each recorded `RequestUsage` carries `provider`, `account`, `api_key` (id), `model`, `request_id` for SaaS-style tenancy.
