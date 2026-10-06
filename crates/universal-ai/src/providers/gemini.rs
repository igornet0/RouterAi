//! Google Gemini adapter (OpenAI-compatible endpoint when available, plus native generateContent).

use async_trait::async_trait;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

use crate::capability::{ModelCapabilities, ProviderCapabilities};
use crate::error::{AiError, AiResult};
use crate::health::HealthStatus;
use crate::http::HttpClient;
use crate::models::ModelInfo;
use crate::provider::{DynProvider, Provider, ProviderCredential};
use crate::types::{
    ChatRequest, ChatResponse, ChatStream, FinishReason, Message, ModelId, ProviderId, RequestId,
    Role, ToolCall,
};
use crate::usage::Usage;

/// Gemini adapter using the Generative Language API.
#[derive(Clone)]
pub struct Gemini {
    /// Static key, or the key bound for one request; `None` for managed-key templates.
    api_key: Option<SecretString>,
    base_url: String,
    http: Arc<HttpClient>,
}

impl std::fmt::Debug for Gemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gemini")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Gemini {
    /// Create with API key.
    pub fn new(api_key: impl Into<SecretString>) -> AiResult<Self> {
        Self::with_http(api_key, HttpClient::new(Default::default())?)
    }

    /// Shared HTTP.
    pub fn with_http(api_key: impl Into<SecretString>, http: HttpClient) -> AiResult<Self> {
        Ok(Self {
            api_key: Some(api_key.into()),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            http: Arc::new(http),
        })
    }

    /// Keyless template for managed keys (bound per request).
    pub(crate) fn template(base_url: Option<&str>, http: HttpClient) -> AiResult<Self> {
        Ok(Self {
            api_key: None,
            base_url: base_url
                .unwrap_or("https://generativelanguage.googleapis.com/v1beta")
                .into(),
            http: Arc::new(http),
        })
    }

    /// Credential for this instance; never falls back to another key.
    fn credential(&self) -> AiResult<&SecretString> {
        self.api_key
            .as_ref()
            .ok_or_else(|| AiError::Authentication {
                provider: Some(ProviderId::gemini()),
                message: "no API key bound to this provider — add an active key".into(),
            })
    }

    /// POST/GET with the key in the `x-goog-api-key` header (never in the URL,
    /// where it would leak into error messages and proxy logs).
    async fn request(&self, method: Method, url: &str, body: Option<&Value>) -> AiResult<Value> {
        let key = self.credential()?;
        let (raw, _, _): (Value, _, _) = self
            .http
            .request_json(
                &ProviderId::gemini(),
                method,
                url,
                None,
                &[("x-goog-api-key", key.expose_secret())],
                body,
            )
            .await
            .map_err(|e| e.redact_secret(key))?;
        Ok(raw)
    }

    /// Test helper.
    pub fn with_base_url(
        api_key: impl Into<SecretString>,
        base_url: impl Into<String>,
        http: HttpClient,
    ) -> AiResult<Self> {
        Ok(Self {
            api_key: Some(api_key.into()),
            base_url: base_url.into(),
            http: Arc::new(http),
        })
    }

    /// Canonical request → `generateContent` body.
    fn to_wire(request: &ChatRequest) -> Value {
        let mut system_parts = Vec::new();
        let mut contents: Vec<Value> = Vec::new();
        // functionResponse is matched by function name, so remember call id → name.
        let mut call_names: HashMap<&str, &str> = HashMap::new();
        for m in &request.messages {
            match m.role {
                Role::System => system_parts.push(m.content.to_plain_text()),
                Role::User => {
                    let text = m.content.to_plain_text();
                    if !text.is_empty() {
                        push_parts(&mut contents, "user", vec![json!({ "text": text })]);
                    }
                }
                Role::Assistant => {
                    let mut parts = Vec::new();
                    let text = m.content.to_plain_text();
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                    for call in &m.tool_calls {
                        call_names.insert(call.id.as_str(), call.function.name.as_str());
                        let args = call
                            .arguments_json()
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}));
                        parts.push(json!({
                            "functionCall": { "name": call.function.name, "args": args }
                        }));
                    }
                    if !parts.is_empty() {
                        push_parts(&mut contents, "model", parts);
                    }
                }
                Role::Tool => {
                    let name = m
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| call_names.get(id).copied())
                        .unwrap_or("unknown");
                    let response =
                        function_response_payload(&m.content.to_plain_text(), m.is_error);
                    // All responses for one model turn go into a single user turn.
                    push_parts(
                        &mut contents,
                        "user",
                        vec![json!({
                            "functionResponse": { "name": name, "response": response }
                        })],
                    );
                }
            }
        }

        let mut body = json!({ "contents": contents });
        if !system_parts.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
        }
        if !request.tools.is_empty() {
            let declarations: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    let mut decl = json!({ "name": t.function.name });
                    if let Some(desc) = &t.function.description {
                        decl["description"] = json!(desc);
                    }
                    if let Some(params) = gemini_schema(&t.function.parameters) {
                        decl["parameters"] = params;
                    }
                    decl
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": declarations }]);
        }

        let mut generation = serde_json::Map::new();
        if let Some(t) = request.temperature {
            generation.insert("temperature".into(), json!(t));
        }
        if let Some(m) = request.max_tokens {
            generation.insert("maxOutputTokens".into(), json!(m));
        }
        if let Some(p) = request.top_p {
            generation.insert("topP".into(), json!(p));
        }
        if !request.stop.is_empty() {
            generation.insert("stopSequences".into(), json!(request.stop));
        }
        if !generation.is_empty() {
            body["generationConfig"] = Value::Object(generation);
        }
        body
    }

    /// First candidate → (text, function calls, finish reason).
    fn parse_candidate(raw: &Value) -> (String, Vec<ToolCall>, FinishReason) {
        let mut text = String::new();
        let mut calls = Vec::new();
        for part in raw
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                let Some(name) = fc.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                // Gemini may omit call ids; mint unique ones so results can be correlated.
                let id = fc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                calls.push(ToolCall::function(id, name, &args));
            }
        }
        let finish = if !calls.is_empty() {
            FinishReason::ToolCalls
        } else {
            match raw
                .pointer("/candidates/0/finishReason")
                .and_then(|f| f.as_str())
            {
                None | Some("STOP") => FinishReason::Stop,
                Some("MAX_TOKENS") => FinishReason::Length,
                Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII") => {
                    FinishReason::ContentFilter
                }
                Some(other) => FinishReason::Other(other.to_string()),
            }
        };
        (text, calls, finish)
    }
}

/// `usageMetadata` → canonical usage. Gemini reports thinking tokens
/// (`thoughtsTokenCount`) and tool-use prompt tokens outside
/// `candidatesTokenCount` / `promptTokenCount`, yet bills them as output / input:
/// both are added back. Non-text modalities and any remainder of
/// `totalTokenCount` that no field explains are kept as unpriceable categories.
fn gemini_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let thoughts = n("thoughtsTokenCount");
    let prompt = n("promptTokenCount") + n("toolUsePromptTokenCount");
    let completion = n("candidatesTokenCount") + thoughts;
    let cached = n("cachedContentTokenCount");
    let mut usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: u
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(prompt + completion),
        cached_tokens: (cached > 0).then_some(cached),
        cache_creation_tokens: None,
        reasoning_tokens: (thoughts > 0).then_some(thoughts),
        other_tokens: Default::default(),
    };
    for (field, prefix) in [
        ("promptTokensDetails", "input"),
        ("candidatesTokensDetails", "output"),
    ] {
        for detail in u.get(field).and_then(Value::as_array).into_iter().flatten() {
            let modality = detail
                .get("modality")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN");
            let count = detail
                .get("tokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if modality != "TEXT" && count > 0 {
                usage
                    .other_tokens
                    .insert(format!("{prefix}_{}", modality.to_ascii_lowercase()), count);
            }
        }
    }
    let unexplained = usage.total_tokens.saturating_sub(prompt + completion);
    if unexplained > 0 {
        usage
            .other_tokens
            .insert("unexplained_total".into(), unexplained);
    }
    Some(usage)
}

/// Append parts, merging into the previous content when the role repeats.
fn push_parts(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

/// `functionResponse.response` must be a JSON object.
fn function_response_payload(content: &str, is_error: bool) -> Value {
    let parsed = serde_json::from_str::<Value>(content).unwrap_or_else(|_| json!(content));
    match parsed {
        Value::Object(map) if !is_error || map.contains_key("error") => Value::Object(map),
        other if is_error => json!({ "error": other }),
        other => json!({ "content": other }),
    }
}

/// Gemini accepts an OpenAPI subset of JSON Schema: drop unsupported keywords and
/// omit object schemas without properties (rejected as "should be non-empty").
fn gemini_schema(schema: &Value) -> Option<Value> {
    fn strip(v: &Value) -> Value {
        match v {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| {
                        !matches!(k.as_str(), "$schema" | "$id" | "additionalProperties")
                    })
                    .map(|(k, v)| (k.clone(), strip(v)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(strip).collect()),
            other => other.clone(),
        }
    }
    let cleaned = strip(schema);
    let has_properties = cleaned
        .get("properties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| !p.is_empty());
    let is_object = cleaned.get("type").and_then(|t| t.as_str()) == Some("object");
    if !cleaned.is_object() || (is_object && !has_properties) {
        return None;
    }
    Some(cleaned)
}

#[async_trait]
impl Provider for Gemini {
    fn id(&self) -> ProviderId {
        ProviderId::gemini()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            chat: true,
            streaming: false, // native stream can be added later
            embeddings: true,
            model_list: true,
            tool_calling: true,
            // `response_format` is not mapped to `responseSchema` by this adapter.
            structured_output: false,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> AiResult<Vec<ModelInfo>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let raw = self.request(Method::GET, &url, None).await?;
        let models = raw
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(models
            .into_iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let id = name.trim_start_matches("models/").to_string();
                Some(ModelInfo {
                    id: ModelId::new(id.clone()),
                    provider: ProviderId::gemini(),
                    name: Some(id),
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities::chat_default(),
                    pricing: None,
                })
            })
            .collect())
    }

    async fn chat(&self, request: ChatRequest) -> AiResult<ChatResponse> {
        let request_id = RequestId::new();
        let model = request.model.as_str();
        let url = format!(
            "{}/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            model,
        );

        let body = Self::to_wire(&request);
        let raw = self.request(Method::POST, &url, Some(&body)).await?;

        let (text, tool_calls, finish_reason) = Self::parse_candidate(&raw);

        let usage = raw.get("usageMetadata").and_then(gemini_usage);

        Ok(ChatResponse {
            request_id,
            model: request.model,
            message: Message::assistant_tool_calls(text, tool_calls),
            finish_reason: Some(finish_reason),
            usage,
            cost: None,
            raw: Some(raw),
        })
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AiResult<ChatStream> {
        Err(AiError::UnsupportedCapability {
            capability: crate::capability::Capability::Streaming,
            provider: Some(ProviderId::gemini()),
        })
    }

    async fn health(&self) -> AiResult<HealthStatus> {
        let started = Instant::now();
        match self.list_models().await {
            Ok(_) => Ok(HealthStatus::ok(started.elapsed().as_millis() as u64)),
            Err(err) => Ok(HealthStatus::down(err.to_string())),
        }
    }

    fn with_credential(&self, credential: &ProviderCredential) -> AiResult<DynProvider> {
        let mut bound = self.clone();
        bound.api_key = Some(credential.secret().clone());
        if let Some(url) = credential.base_url() {
            bound.base_url = url.to_string();
        }
        Ok(Arc::new(bound))
    }
}
