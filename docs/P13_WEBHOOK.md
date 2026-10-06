# P13 — Webhook EventSource / EventSink

Universal HTTP ingress and outbound callbacks — no Telegram types in `routerai` core.

```
POST /api/v1/webhooks/{event_type}
        ↓
  WebhookSource → Event (source=webhook)
        ↓
     EventBus
        ↓
     Handler → Agent → Tools → Result
        ↓
  agent.completed / agent.failed
        ↓
  WebhookSink → HTTP POST (targets + reply_to)
```

## Crate layout

```
routerai              # EventSource / EventSink traits, SinkRegistry
routerai-adapters
├── webhook/          # source + HTTP sink
├── http/             # shared outbound envelope
└── (telegram — P14)
routerai-server       # REST wiring
```

## Ingress

Preferred:

```bash
curl -X POST http://127.0.0.1:8080/api/v1/webhooks/message.received \
  -H 'Content-Type: application/json' \
  -d '{"customer":"123","message":"Хочу узнать цену"}'
```

Normalizes to event type `webhook.message.received` (path `message.received` → `webhook.*`).

Also:

| Route | Role |
|-------|------|
| `POST /api/v1/webhooks` | Default type `webhook.message.received` |
| `POST /api/v1/webhooks/{*event_type}` | Normalized webhook event |
| `POST /api/v1/events/{event_type}` | Same ingress (alias) |
| `POST /api/v1/events` | Raw console emit (unchanged) |

### Headers

| Header | Purpose |
|--------|---------|
| `X-RouterAi-Webhook-Secret` / `Authorization: Bearer …` | Optional auth if `ROUTERAI_WEBHOOK_SECRET` is set |
| `X-Reply-To` | Callback URL for `agent.completed` / `agent.failed` |
| `X-Correlation-Id` | Correlation |
| `X-RouterAi-Account` | Account metadata |

`reply_to` may also be a field in the JSON body.

## Sink (outbound)

Register durable targets:

```bash
curl -X POST http://127.0.0.1:8080/api/v1/webhook-targets \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com/hooks/routerai","event_types":["agent.completed","agent.failed"]}'
```

- `event_types` empty → all events  
- suffix `.*` supported (`agent.*`)  
- secret → sent as `X-RouterAi-Sink-Secret`

Payload is a JSON envelope: `id`, `event_type`, `source`, `timestamp`, `payload`, correlation/causation, metadata.

## End-to-end proof

1. Publish a Sales Agent  
2. Handler on `webhook.message.received` → AgentRun  
3. `curl` webhook ingress  
4. Agent runs; `agent.completed` appears in Events / Runs  
5. Sink POSTs result to your callback (or `X-Reply-To`)

## Out of scope

- P14 Telegram adapter  
- Signed HMAC body verification (secret header only for now)  
- Durable outbox / retries across process restart  
