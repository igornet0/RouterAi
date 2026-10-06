//! REST + SSE API handlers.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use routerai::{
    Agent, AgentId, Dataset, Event, EventId, Handler, HandlerId, PublishOptions, RouterRuntime,
    RunId, Schedule, SinkTarget, TestCase,
};
use routerai_adapters::webhook::{
    authorize_ingress, ingress_event, CreateSinkTargetRequest, WebhookIngressOptions,
};
use serde::Deserialize;
use serde_json::Value;
use universal_ai::{
    provider_catalog, AccountId, AddKeyRequest, KeyId, ProviderId, RequestId, SecretString,
};

use crate::credentials::persist_from_client;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<RouterRuntime>,
    /// Optional shared secret for webhook ingress (`ROUTERAI_WEBHOOK_SECRET`).
    pub webhook_secret: Option<String>,
    /// Path to credentials.json for account/key metadata.
    pub credentials_path: PathBuf,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/doctor", get(doctor))
        .route("/api/v1/dashboard", get(dashboard))
        .route("/api/v1/settings/kill-switch", get(get_kill).post(set_kill))
        .route("/api/v1/audit", get(list_audit))
        .route("/api/v1/providers", get(list_providers))
        .route("/api/v1/accounts", get(list_accounts).post(create_account))
        .route("/api/v1/accounts/{id}/credit-budget", post(set_credit_budget))
        .route("/api/v1/keys", get(list_keys).post(create_key))
        .route("/api/v1/keys/{id}", delete(delete_key))
        .route("/api/v1/balances", get(list_balances))
        .route("/api/v1/ai-requests", get(list_ai_requests))
        .route("/api/v1/ai-requests/{id}", get(get_ai_request))
        .route(
            "/api/v1/ai-requests/{id}/importance",
            patch(set_ai_request_importance),
        )
        .route("/api/v1/events", post(emit_event).get(list_events))
        .route(
            "/api/v1/events/{event_type}",
            get(get_event).post(webhook_ingress),
        )
        // P13 webhook EventSource — preferred ingress
        .route("/api/v1/webhooks", post(webhook_ingress_default))
        .route("/api/v1/webhooks/{*event_type}", post(webhook_ingress))
        .route(
            "/api/v1/webhook-targets",
            get(list_webhook_targets).post(create_webhook_target),
        )
        .route("/api/v1/webhook-targets/{id}", delete(delete_webhook_target))
        .route("/api/v1/handlers", post(upsert_handler).get(list_handlers))
        .route("/api/v1/handlers/{id}", get(get_handler).delete(delete_handler))
        .route("/api/v1/agents", post(create_agent).get(list_agents))
        .route(
            "/api/v1/agents/{id}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/api/v1/agents/{id}/validate", post(validate_agent))
        .route("/api/v1/agents/{id}/publish", post(publish_agent))
        .route("/api/v1/agents/{id}/revise", post(revise_agent))
        .route("/api/v1/agents/{id}/pause", post(pause_agent))
        .route("/api/v1/agents/{id}/resume", post(resume_agent))
        .route("/api/v1/agents/{id}/archive", post(archive_agent))
        .route("/api/v1/agents/{id}/testing", post(mark_testing))
        .route("/api/v1/agents/{id}/ready", post(mark_ready))
        .route("/api/v1/agents/{id}/rollback", post(rollback_agent))
        .route("/api/v1/agents/{id}/versions", get(agent_versions))
        .route("/api/v1/agents/{id}/runs", post(start_run))
        .route("/api/v1/agents/{id}/playground", post(playground))
        .route("/api/v1/runs", get(list_runs))
        .route("/api/v1/runs/{id}", get(get_run))
        .route("/api/v1/runs/{id}/cancel", post(cancel_run))
        .route("/api/v1/runs/{id}/retry", post(retry_run))
        .route("/api/v1/tools", get(list_tools))
        .route("/api/v1/schedules", post(upsert_schedule).get(list_schedules))
        .route("/api/v1/schedules/tick", post(tick_schedules))
        .route("/api/v1/test-cases", post(upsert_case).get(list_cases))
        .route("/api/v1/test-cases/{id}", delete(delete_case))
        .route("/api/v1/test-cases/{id}/run", post(run_case))
        .route("/api/v1/datasets", post(upsert_dataset).get(list_datasets))
        .route("/api/v1/datasets/{id}/regression", post(run_regression))
        .route("/api/v1/ws/events", get(sse_events))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(serde_json::json!({ "ok": true, "service": "routerai" }))
}

async fn doctor(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.runtime.doctor().await).unwrap_or(Value::Null))
}

async fn dashboard(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let snap = state.runtime.dashboard().await.map_err(map_err)?;
    Ok(Json(serde_json::to_value(snap).unwrap()))
}

async fn get_kill(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({ "kill_switch": state.runtime.kill_switch() }))
}

#[derive(Deserialize)]
struct KillBody {
    enabled: bool,
    #[serde(default = "default_actor")]
    actor: String,
}

fn default_actor() -> String {
    "console".into()
}

async fn set_kill(State(state): State<AppState>, Json(body): Json<KillBody>) -> Json<Value> {
    let on = state
        .runtime
        .control()
        .set_kill_switch(body.enabled, &body.actor)
        .await;
    Json(serde_json::json!({ "kill_switch": on }))
}

async fn list_audit(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({
        "entries": state.runtime.audit().list(100).await
    }))
}

fn require_ai(
    state: &AppState,
) -> Result<&universal_ai::AiClient, (axum::http::StatusCode, String)> {
    state.runtime.ai().ok_or_else(|| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "AiClient not configured".into(),
        )
    })
}

async fn persist_creds(state: &AppState) -> Result<(), (axum::http::StatusCode, String)> {
    let ai = require_ai(state)?;
    persist_from_client(ai, &state.credentials_path)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn list_providers() -> Json<Value> {
    Json(serde_json::json!({ "providers": provider_catalog() }))
}

async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let accounts = ai.accounts().list_accounts().await;
    Ok(Json(serde_json::json!({ "accounts": accounts })))
}

#[derive(Deserialize)]
struct CreateAccountBody {
    provider: String,
    name: String,
}

async fn create_account(
    State(state): State<AppState>,
    Json(body): Json<CreateAccountBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let account = ai
        .accounts()
        .add_account(ProviderId::new(body.provider), body.name)
        .await
        .map_err(map_ai_err)?;
    persist_creds(&state).await?;
    Ok(Json(serde_json::to_value(account).unwrap()))
}

#[derive(Deserialize)]
struct CreditBudgetBody {
    /// USD amount as string, or null to clear.
    credit_budget: Option<String>,
}

async fn set_credit_budget(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreditBudgetBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let budget = match body.credit_budget.as_deref() {
        None | Some("") => None,
        Some(s) => Some(
            s.parse::<rust_decimal::Decimal>()
                .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("invalid budget: {e}")))?,
        ),
    };
    let account = ai
        .accounts()
        .set_credit_budget(&AccountId::new(id), budget)
        .await
        .map_err(map_ai_err)?;
    persist_creds(&state).await?;
    Ok(Json(serde_json::to_value(account).unwrap()))
}

async fn list_balances(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let balances = ai.probe_balances().await;
    Ok(Json(serde_json::json!({ "balances": balances })))
}

#[derive(Deserialize)]
struct AiRequestsQuery {
    #[serde(default = "default_ai_requests_limit")]
    limit: usize,
}

fn default_ai_requests_limit() -> usize {
    50
}

async fn list_ai_requests(
    State(state): State<AppState>,
    Query(q): Query<AiRequestsQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let limit = q.limit.clamp(1, 500);
    let requests = ai.list_ai_requests(limit).await.map_err(map_ai_err)?;
    Ok(Json(serde_json::json!({ "requests": requests })))
}

async fn get_ai_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let request_id = id
        .parse::<RequestId>()
        .map_err(map_ai_err)?;
    match ai.get_ai_request(&request_id).await.map_err(map_ai_err)? {
        Some(row) => Ok(Json(serde_json::to_value(row).unwrap())),
        None => Err((
            axum::http::StatusCode::NOT_FOUND,
            format!("request {id}"),
        )),
    }
}

#[derive(Deserialize)]
struct ImportanceBody {
    importance: Option<u8>,
}

async fn set_ai_request_importance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ImportanceBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let request_id = id.parse::<RequestId>().map_err(map_ai_err)?;
    let row = ai
        .set_request_importance(&request_id, body.importance)
        .await
        .map_err(map_ai_err)?;
    Ok(Json(serde_json::to_value(row).unwrap()))
}

async fn list_keys(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let keys = ai.keys().list_keys().await;
    Ok(Json(serde_json::json!({ "keys": keys })))
}

#[derive(Deserialize)]
struct CreateKeyBody {
    provider: String,
    #[serde(default)]
    account_id: Option<String>,
    secret: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

async fn create_key(
    State(state): State<AppState>,
    Json(body): Json<CreateKeyBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let provider = ProviderId::new(body.provider);
    let account_id = if let Some(id) = body.account_id {
        AccountId::new(id)
    } else {
        // Auto-create a default account for this provider if none supplied.
        let existing = ai.accounts().list_accounts().await;
        if let Some(acc) = existing.into_iter().find(|a| a.provider == provider) {
            acc.id
        } else {
            ai.accounts()
                .add_account(provider.clone(), format!("{} default", provider))
                .await
                .map_err(map_ai_err)?
                .id
        }
    };

    let info = ai
        .keys()
        .add_key(AddKeyRequest {
            provider: provider.clone(),
            account_id,
            secret: SecretString::new(body.secret.into()),
            name: body.name,
            base_url: body.base_url.clone(),
        })
        .await
        .map_err(map_ai_err)?;

    ai.sync_provider_from_key(&info, info.base_url.as_deref())
        .await
        .map_err(map_ai_err)?;
    persist_creds(&state).await?;
    Ok(Json(serde_json::to_value(info).unwrap()))
}

async fn delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ai = require_ai(&state)?;
    let key_id = KeyId::new(id);
    let info = ai.keys().get_key(&key_id).await;
    ai.keys().remove_key(&key_id).await.map_err(map_ai_err)?;
    if let Some(info) = info {
        // Drop provider only if no other active key remains for it.
        let still = ai
            .keys()
            .list_keys()
            .await
            .into_iter()
            .any(|k| k.provider == info.provider && k.status == universal_ai::KeyStatus::Active);
        if !still {
            ai.unregister_provider(&info.provider);
        }
    }
    persist_creds(&state).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn map_ai_err(err: universal_ai::AiError) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    use universal_ai::AiError;
    let status = match &err {
        AiError::NotFound { .. } => StatusCode::NOT_FOUND,
        AiError::InvalidRequest { .. }
        | AiError::UnsupportedModel { .. }
        | AiError::Config { .. } => StatusCode::BAD_REQUEST,
        AiError::BudgetExceeded { .. } | AiError::InsufficientBalance { .. } => {
            StatusCode::PAYMENT_REQUIRED
        }
        AiError::Authentication { .. } | AiError::Authorization { .. } => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, err.to_string())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, "authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
}

async fn webhook_ingress_default(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    webhook_ingress_inner(state, headers, "message.received", payload).await
}

async fn webhook_ingress(
    State(state): State<AppState>,
    Path(event_type): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    webhook_ingress_inner(state, headers, &event_type, payload).await
}

async fn webhook_ingress_inner(
    state: AppState,
    headers: HeaderMap,
    event_type: &str,
    payload: Value,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let secret_hdr = header_str(&headers, "x-routerai-webhook-secret");
    let bearer = bearer_token(&headers);
    if !authorize_ingress(state.webhook_secret.as_deref(), secret_hdr, bearer) {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid webhook secret".into(),
        ));
    }

    let reply_to = header_str(&headers, "x-reply-to")
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("reply_to")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let account = header_str(&headers, "x-routerai-account").map(str::to_string);
    let correlation_id = header_str(&headers, "x-correlation-id").map(str::to_string);

    let event = ingress_event(
        event_type,
        payload,
        WebhookIngressOptions {
            reply_to,
            account,
            correlation_id,
        },
    );
    let runs = state.runtime.emit(event.clone()).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({
        "event": event,
        "runs": runs,
    })))
}

async fn list_webhook_targets(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({
        "targets": state.runtime.sinks().list_targets().await
    }))
}

async fn create_webhook_target(
    State(state): State<AppState>,
    Json(body): Json<CreateSinkTargetRequest>,
) -> Result<Json<SinkTarget>, (axum::http::StatusCode, String)> {
    if body.url.trim().is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "url is required".into(),
        ));
    }
    let target = state
        .runtime
        .upsert_sink_target(body.into_target())
        .await
        .map_err(map_err)?;
    Ok(Json(target))
}

async fn delete_webhook_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .delete_sink_target(&id)
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct EmitBody {
    event_type: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    account: Option<String>,
}

fn default_source() -> String {
    "api".into()
}

async fn emit_event(
    State(state): State<AppState>,
    Json(body): Json<EmitBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let mut event = Event::new(body.event_type, body.source, body.payload);
    if let Some(account) = body.account {
        event = event.with_account(account);
    }
    let runs = state.runtime.emit(event.clone()).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({ "event": event, "runs": runs })))
}

async fn list_events(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({ "events": state.runtime.events().list(200).await }))
}

async fn get_event(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let ev = state
        .runtime
        .events()
        .get(&EventId::from_string(id))
        .await
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "event not found".into()))?;
    Ok(Json(serde_json::to_value(ev).unwrap()))
}

async fn upsert_handler(
    State(state): State<AppState>,
    Json(handler): Json<Handler>,
) -> Result<Json<Handler>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .upsert_handler(handler.clone())
        .await
        .map_err(map_err)?;
    Ok(Json(handler))
}

async fn list_handlers(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({ "handlers": state.runtime.handlers().list().await }))
}

async fn get_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Handler>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .handlers()
        .get(&HandlerId::from_string(id))
        .await
        .map(Json)
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "handler not found".into()))
}

async fn delete_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .delete_handler(&HandlerId::from_string(id))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct AgentListQuery {
    #[serde(default)]
    latest: bool,
}

async fn create_agent(
    State(state): State<AppState>,
    Json(agent): Json<Agent>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let agent = state.runtime.upsert_agent(agent).await.map_err(map_err)?;
    Ok(Json(agent))
}

async fn list_agents(
    State(state): State<AppState>,
    Query(q): Query<AgentListQuery>,
) -> Json<Value> {
    let agents = if q.latest {
        state.runtime.agents().list_latest().await
    } else {
        state.runtime.agents().list().await
    };
    Json(serde_json::json!({ "agents": agents }))
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .agents()
        .get(&AgentId::from_string(id))
        .await
        .map(Json)
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "agent not found".into()))
}

async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut agent): Json<Agent>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    agent.id = AgentId::from_string(id);
    let agent = state.runtime.upsert_agent(agent).await.map_err(map_err)?;
    Ok(Json(agent))
}

async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .delete_agent(&AgentId::from_string(id))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn validate_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let report = state
        .runtime
        .validate_publish(&AgentId::from_string(id))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::to_value(report).unwrap()))
}

#[derive(Deserialize)]
struct PublishBody {
    #[serde(default)]
    override_tests: bool,
    #[serde(default = "default_actor")]
    actor: String,
}

impl Default for PublishBody {
    fn default() -> Self {
        Self {
            override_tests: false,
            actor: default_actor(),
        }
    }
}

async fn publish_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<PublishBody>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let opts = body.map(|b| b.0).unwrap_or_default();
    let result = state
        .runtime
        .control()
        .publish(
            &AgentId::from_string(id),
            PublishOptions {
                override_tests: opts.override_tests,
                actor: opts.actor,
            },
        )
        .await
        .map_err(map_err)?;
    if result.agent.is_none() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::to_string(&result.validation)
                .unwrap_or_else(|_| "validation failed".into()),
        ));
    }
    Ok(Json(serde_json::to_value(result).unwrap()))
}

async fn revise_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    Ok(Json(
        state
            .runtime
            .revise_agent(&AgentId::from_string(id))
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct ActorBody {
    #[serde(default = "default_actor")]
    actor: String,
}

async fn pause_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ActorBody>>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let actor = body.map(|b| b.0.actor).unwrap_or_else(default_actor);
    Ok(Json(
        state
            .runtime
            .control()
            .pause(&AgentId::from_string(id), &actor)
            .await
            .map_err(map_err)?,
    ))
}

async fn resume_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ActorBody>>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let actor = body.map(|b| b.0.actor).unwrap_or_else(default_actor);
    Ok(Json(
        state
            .runtime
            .control()
            .resume(&AgentId::from_string(id), &actor)
            .await
            .map_err(map_err)?,
    ))
}

async fn archive_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ActorBody>>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let actor = body.map(|b| b.0.actor).unwrap_or_else(default_actor);
    Ok(Json(
        state
            .runtime
            .control()
            .archive(&AgentId::from_string(id), &actor)
            .await
            .map_err(map_err)?,
    ))
}

async fn mark_testing(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ActorBody>>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let actor = body.map(|b| b.0.actor).unwrap_or_else(default_actor);
    Ok(Json(
        state
            .runtime
            .control()
            .mark_testing(&AgentId::from_string(id), &actor)
            .await
            .map_err(map_err)?,
    ))
}

async fn mark_ready(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ActorBody>>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    let actor = body.map(|b| b.0.actor).unwrap_or_else(default_actor);
    Ok(Json(
        state
            .runtime
            .control()
            .mark_ready(&AgentId::from_string(id), &actor)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct RollbackBody {
    to_version: u32,
    #[serde(default = "default_actor")]
    actor: String,
}

async fn rollback_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Result<Json<Agent>, (axum::http::StatusCode, String)> {
    Ok(Json(
        state
            .runtime
            .control()
            .rollback(&AgentId::from_string(id), body.to_version, &body.actor)
            .await
            .map_err(map_err)?,
    ))
}

async fn agent_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let agent = state
        .runtime
        .agents()
        .get(&AgentId::from_string(id.clone()))
        .await
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "agent not found".into()))?;
    let versions = state.runtime.agents().versions(&agent.lineage_id).await;
    Ok(Json(serde_json::json!({ "versions": versions })))
}

#[derive(Deserialize)]
struct RunBody {
    #[serde(default)]
    input: Value,
    /// interactive = playground/manual; automated = handler gate
    #[serde(default = "default_interactive")]
    mode: String,
}

fn default_interactive() -> String {
    "interactive".into()
}

async fn start_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let mode = if body.mode == "automated" {
        routerai::RunMode::Automated
    } else {
        routerai::RunMode::Interactive
    };
    let run = state
        .runtime
        .start_run_with_mode(&AgentId::from_string(id), body.input, None, mode)
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

#[derive(Deserialize)]
struct PlaygroundBody {
    message: String,
    #[serde(default)]
    context: Option<Value>,
}

async fn playground(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PlaygroundBody>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let run = state
        .runtime
        .playground_message(&AgentId::from_string(id), body.message, body.context)
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

async fn list_runs(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let runs = state.runtime.store().list_runs(100).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({ "runs": runs })))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let run = state
        .runtime
        .store()
        .get_run(&RunId::from_string(id))
        .await
        .map_err(map_err)?
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "run not found".into()))?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

async fn cancel_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .cancel_run(&RunId::from_string(id))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({ "cancelled": true })))
}

async fn retry_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let run = state
        .runtime
        .retry_run(&RunId::from_string(id))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

async fn list_tools(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({
        "tools": state.runtime.tools().registry().list().await
    }))
}

async fn upsert_schedule(
    State(state): State<AppState>,
    Json(schedule): Json<Schedule>,
) -> Result<Json<Schedule>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .upsert_schedule(schedule.clone())
        .await
        .map_err(map_err)?;
    Ok(Json(schedule))
}

async fn list_schedules(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({ "schedules": state.runtime.scheduler().list().await }))
}

async fn tick_schedules(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let runs = state.runtime.tick_schedules().await.map_err(map_err)?;
    Ok(Json(serde_json::json!({ "runs": runs })))
}

async fn upsert_case(
    State(state): State<AppState>,
    Json(case): Json<TestCase>,
) -> Result<Json<TestCase>, (axum::http::StatusCode, String)> {
    Ok(Json(
        state
            .runtime
            .upsert_test_case(case)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct CaseQuery {
    agent_id: Option<String>,
}

async fn list_cases(
    State(state): State<AppState>,
    Query(q): Query<CaseQuery>,
) -> Json<Value> {
    let agent = q.agent_id.map(AgentId::from_string);
    let cases = state.runtime.tests().list_cases(agent.as_ref()).await;
    Json(serde_json::json!({ "cases": cases }))
}

async fn delete_case(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    state
        .runtime
        .delete_test_case(&id)
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn run_case(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let case = state
        .runtime
        .tests()
        .get_case(&id)
        .await
        .ok_or_else(|| (axum::http::StatusCode::NOT_FOUND, "case not found".into()))?;
    let (run, report) = state.runtime.run_test(&case).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({ "run": run, "evaluation": report })))
}

async fn upsert_dataset(
    State(state): State<AppState>,
    Json(ds): Json<Dataset>,
) -> Result<Json<Dataset>, (axum::http::StatusCode, String)> {
    Ok(Json(
        state
            .runtime
            .upsert_dataset(ds)
            .await
            .map_err(map_err)?,
    ))
}

async fn list_datasets(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!({ "datasets": state.runtime.tests().list_datasets().await }))
}

async fn run_regression(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let report = state.runtime.run_regression(&id).await.map_err(map_err)?;
    Ok(Json(serde_json::to_value(report).unwrap()))
}

async fn sse_events(
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = state.runtime.events().subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                    return Some((Ok(SseEvent::default().event("event").data(data)), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn map_err(err: routerai::RouterError) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    use routerai::RouterError;
    let status = match &err {
        RouterError::NotFound(_) => StatusCode::NOT_FOUND,
        RouterError::Forbidden(_) | RouterError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        RouterError::Invalid(_) | RouterError::Policy(_) => StatusCode::BAD_REQUEST,
        RouterError::BudgetExceeded(_) => StatusCode::PAYMENT_REQUIRED,
        RouterError::Cancelled => StatusCode::CONFLICT,
        RouterError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, err.to_string())
}
