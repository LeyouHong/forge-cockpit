//! A minimal browser UI for driving the Forge agent over HTTP.
//!
//! This crate is a *second consumer* of the [`forge_api::API`] facade (the TUI
//! being the first). It exposes a small REST surface plus a Server-Sent-Events
//! endpoint that streams [`forge_domain::ChatResponse`] events to a single-page
//! frontend.
//!
//! Security note: the agent executes shell commands and edits files on the host.
//! The server binds to loopback and gates every `/api/*` route (and the page
//! itself) behind a per-run bearer token printed at startup.

mod dto;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use forge_api::API;
use forge_domain::{
    AgentId, Attachment, AttachmentContent, AuthContextRequest, AuthContextResponse, AuthMethod,
    ChatRequest, ChatResponse, ConfigOperation, Context, ContextMessage, Conversation,
    ConversationId, Event as DomainEvent, Image, McpHttpServer, McpOAuthSetting, McpServerConfig,
    McpStdioServer, ModelId, ProviderId, Role, Scope, ServerName,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dto::{ChatEventDto, MessageDto};

/// Shared application state handed to every request handler.
struct AppState<A> {
    api: Arc<A>,
    /// Per-run bearer token that gates the UI and API.
    token: Arc<String>,
    /// Serializes MCP config read-modify-write so concurrent add/delete can't
    /// corrupt the config file.
    config_lock: Arc<tokio::sync::Mutex<()>>,
}

// Manual `Clone` so we don't require `A: Clone` (only the `Arc`s are cloned).
impl<A> Clone for AppState<A> {
    fn clone(&self) -> Self {
        Self { api: self.api.clone(), token: self.token.clone(), config_lock: self.config_lock.clone() }
    }
}

/// Starts the web server and blocks until it shuts down.
///
/// # Arguments
/// * `api` - The initialised Forge API facade, shared with the caller.
/// * `addr` - Address to bind. Callers should pass a loopback address.
pub async fn serve<A>(api: Arc<A>, addr: SocketAddr, open_browser: bool) -> anyhow::Result<()>
where
    A: API + 'static,
{
    // A fresh random token per run. UUID v4 gives us ample entropy without a
    // new dependency.
    let token = ConversationId::generate().into_string();
    let state = AppState { api, token: Arc::new(token), config_lock: Arc::new(tokio::sync::Mutex::new(())) };

    // `/api/*` routes require the bearer token.
    let api_routes = Router::new()
        .route(
            "/api/conversations",
            get(list_conversations::<A>).post(create_conversation::<A>),
        )
        .route(
            "/api/conversations/{id}",
            get(get_conversation::<A>).delete(delete_conversation::<A>),
        )
        .route("/api/conversations/{id}/rename", post(rename_conversation::<A>))
        .route("/api/conversations/{id}/messages", get(get_messages::<A>))
        .route("/api/agents", get(list_agents::<A>).post(set_agent::<A>))
        .route("/api/models", get(list_models::<A>).post(set_model::<A>))
        .route("/api/providers", get(list_providers::<A>))
        .route("/api/providers/apikey", post(provider_apikey::<A>))
        .route("/api/providers/device", post(provider_device::<A>))
        .route("/api/providers/{id}", delete(remove_provider::<A>))
        .route("/api/mcp", get(list_mcp::<A>).post(add_mcp::<A>))
        .route("/api/mcp/{name}", delete(delete_mcp::<A>))
        .route("/api/chat", post(chat::<A>))
        .route_layer(from_fn_with_state(state.clone(), auth::<A>));

    let app = Router::new()
        .route("/", get(index::<A>))
        .merge(api_routes)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let url = format!("http://{local}/?token={}", state.token);
    tracing::info!("Forge web UI listening on {url}");
    println!("Forge web UI ready. Open:\n  {url}");
    println!(
        "  ⚠ Anyone with this URL/token can run commands and edit files as you. \
         Keep it private; it is valid only for this session."
    );

    // Open the default browser once the server is accepting connections. A short
    // delay avoids the browser racing ahead of `axum::serve`.
    if open_browser {
        let url = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if let Err(err) = open::that(&url) {
                tracing::warn!("Could not open browser automatically: {err}");
            }
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Middleware that enforces the bearer token on `/api/*` routes.
async fn auth<A>(
    State(state): State<AppState<A>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if provided == Some(state.token.as_str()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Query string for the index route (`/?token=...`).
#[derive(Deserialize)]
struct IndexQuery {
    token: Option<String>,
}

/// Serves the single-page frontend, gated by the token query parameter.
///
/// The token is injected into the page so the frontend can authorize its API
/// calls without the user copy-pasting it.
async fn index<A>(
    State(state): State<AppState<A>>,
    Query(query): Query<IndexQuery>,
) -> Response {
    if query.token.as_deref() != Some(state.token.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Html("<h1>Unauthorized</h1><p>Open the URL printed by <code>forge serve</code>, which includes the access token.</p>"),
        )
            .into_response();
    }
    let page = include_str!("index.html").replace("__FORGE_TOKEN__", &state.token);
    Html(page).into_response()
}

/// A lightweight conversation summary for the sidebar.
#[derive(Serialize)]
struct ConversationSummary {
    id: String,
    title: Option<String>,
}

/// `GET /api/conversations` — most recent conversations.
async fn list_conversations<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<ConversationSummary>>, AppError> {
    let conversations = state.api.get_conversations(Some(50)).await?;
    let summaries = conversations
        .into_iter()
        .map(|c| ConversationSummary {
            // Prefer Forge's own generated title; it is populated best-effort
            // and asynchronously, so fall back to a snippet of the first user
            // message (like most chat UIs) when it isn't set yet.
            title: c.title.clone().or_else(|| derive_title(c.context.as_ref())),
            id: c.id.into_string(),
        })
        .collect();
    Ok(Json(summaries))
}

/// Derives a sidebar label from the first user message in a conversation.
///
/// Uses the pre-template raw prompt when available, otherwise strips the
/// `<task>…</task>` wrapper from the rendered content. Returns `None` when the
/// conversation has no user message yet (the frontend then shows "New chat").
fn derive_title(context: Option<&Context>) -> Option<String> {
    let entry = context?
        .messages
        .iter()
        .find(|m| m.message.has_role(Role::User))?;

    let raw = entry
        .message
        .as_value()
        .and_then(|v| v.as_user_prompt())
        .map(|p| p.as_str().to_string())
        .or_else(|| match &entry.message {
            ContextMessage::Text(text) => Some(strip_task(&text.content)),
            _ => None,
        })?;

    // Collapse to a single trimmed line.
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(&text, 48))
}

/// Extracts the inner text of a `<task>…</task>` wrapper, if present.
fn strip_task(s: &str) -> String {
    if let Some(start) = s.find("<task>") {
        let after = &s[start + "<task>".len()..];
        if let Some(end) = after.find("</task>") {
            return after[..end].to_string();
        }
    }
    s.to_string()
}

/// Character-safe truncation (handles multi-byte text like CJK) with an ellipsis.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// `POST /api/conversations` — creates an empty conversation and returns its id.
async fn create_conversation<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<ConversationSummary>, AppError> {
    let conversation = Conversation::generate();
    let id = conversation.id;
    state.api.upsert_conversation(conversation).await?;
    Ok(Json(ConversationSummary { id: id.into_string(), title: None }))
}

/// `GET /api/conversations/{id}` — the full conversation document.
async fn get_conversation<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
) -> Result<Json<Conversation>, AppError> {
    let id = ConversationId::parse(&id)?;
    match state.api.conversation(&id).await? {
        Some(conversation) => Ok(Json(conversation)),
        None => Err(AppError::not_found(format!("conversation '{id}' not found"))),
    }
}

/// Body for `POST /api/conversations/{id}/rename`.
#[derive(Deserialize)]
struct RenameBody {
    title: String,
}

/// `POST /api/conversations/{id}/rename` — sets a conversation's title.
async fn rename_conversation<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let id = ConversationId::parse(&id)?;
    state.api.rename_conversation(&id, body.title).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `DELETE /api/conversations/{id}` — permanently removes a conversation.
async fn delete_conversation<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let id = ConversationId::parse(&id)?;
    state.api.delete_conversation(&id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `GET /api/conversations/{id}/messages` — stored messages for history replay.
async fn get_messages<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<MessageDto>>, AppError> {
    let id = ConversationId::parse(&id)?;
    let conversation = state
        .api
        .conversation(&id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("conversation '{id}' not found")))?;

    let messages = conversation
        .context
        .as_ref()
        .map(|ctx| MessageDto::from_entries(&ctx.messages))
        .unwrap_or_default();
    Ok(Json(messages))
}

/// An agent option for the picker, plus which one is active.
#[derive(Serialize)]
struct AgentDto {
    id: String,
    title: Option<String>,
    active: bool,
}

/// `GET /api/agents` — available agents with the active one flagged.
async fn list_agents<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<AgentDto>>, AppError> {
    let active = state.api.get_active_agent().await;
    let agents = state.api.get_agent_infos().await?;
    let dtos = agents
        .into_iter()
        .map(|a| AgentDto {
            active: active.as_ref() == Some(&a.id),
            id: a.id.to_string(),
            title: a.title,
        })
        .collect();
    Ok(Json(dtos))
}

/// Body for `POST /api/agents`.
#[derive(Deserialize)]
struct SetAgentBody {
    id: String,
}

/// `POST /api/agents` — sets the active agent.
async fn set_agent<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<SetAgentBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.api.set_active_agent(AgentId::new(body.id)).await?;
    Ok(Json(json!({ "ok": true })))
}

/// A model option for the picker, plus which one is active for the session.
#[derive(Serialize)]
struct ModelDto {
    id: String,
    name: Option<String>,
    active: bool,
}

/// `GET /api/models` — available models with the session model flagged.
async fn list_models<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<ModelDto>>, AppError> {
    let active = state.api.get_session_config().await.map(|c| c.model);
    let models = state.api.get_models().await?;
    let dtos = models
        .into_iter()
        .map(|m| ModelDto {
            active: active.as_ref() == Some(&m.id),
            id: m.id.as_str().to_string(),
            name: m.name,
        })
        .collect();
    Ok(Json(dtos))
}

/// Body for `POST /api/models`.
#[derive(Deserialize)]
struct SetModelBody {
    id: String,
}

/// `POST /api/models` — sets the session model (keeping the current provider).
async fn set_model<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<SetModelBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut config = state
        .api
        .get_session_config()
        .await
        .ok_or_else(|| AppError::bad_request("no active session provider to attach the model to"))?;
    config.model = ModelId::new(body.id);
    state
        .api
        .update_config(vec![ConfigOperation::SetSessionConfig(config)])
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Provider authentication
// ---------------------------------------------------------------------------

/// A provider row for the auth panel.
#[derive(Serialize)]
struct ProviderDto {
    id: String,
    configured: bool,
    /// Which login flows this provider supports: `api_key`, `device`, `code`, …
    methods: Vec<String>,
}

fn method_name(m: &AuthMethod) -> &'static str {
    match m {
        AuthMethod::ApiKey => "api_key",
        AuthMethod::OAuthDevice(_) | AuthMethod::CodexDevice(_) => "device",
        AuthMethod::OAuthCode(_) => "code",
        AuthMethod::GoogleAdc => "google_adc",
        AuthMethod::AwsProfile => "aws_profile",
    }
}

/// `GET /api/providers` — all providers with their configured state + flows.
async fn list_providers<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<ProviderDto>>, AppError> {
    let providers = state.api.get_providers().await?;
    let dtos = providers
        .into_iter()
        .map(|p| ProviderDto {
            id: p.id().as_ref().to_string(),
            configured: p.is_configured(),
            methods: p.auth_methods().iter().map(|m| method_name(m).to_string()).collect(),
        })
        .collect();
    Ok(Json(dtos))
}

/// Finds a provider's advertised auth method matching a requested kind.
async fn find_auth_method<A: API>(
    api: &A,
    provider_id: &ProviderId,
    kinds: &[&str],
) -> Result<AuthMethod, AppError> {
    let provider = api.get_provider(provider_id).await?;
    provider
        .auth_methods()
        .iter()
        .find(|m| kinds.contains(&method_name(m)))
        .cloned()
        .ok_or_else(|| AppError::bad_request("provider does not support this login method"))
}

/// Body for `POST /api/providers/apikey`.
#[derive(Deserialize)]
struct ApiKeyBody {
    id: String,
    api_key: String,
}

/// `POST /api/providers/apikey` — stores an API key for a provider.
async fn provider_apikey<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<ApiKeyBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let provider_id = ProviderId::from(body.id);
    let method = find_auth_method(state.api.as_ref(), &provider_id, &["api_key"]).await?;
    let request = match state.api.init_provider_auth(provider_id.clone(), method).await? {
        AuthContextRequest::ApiKey(request) => request,
        _ => return Err(AppError::bad_request("unexpected auth flow for api key")),
    };
    let response = AuthContextResponse::api_key(request, &body.api_key, HashMap::new());
    state
        .api
        .complete_provider_auth(provider_id, response, Duration::from_secs(60))
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// Body for `POST /api/providers/device`.
#[derive(Deserialize)]
struct DeviceBody {
    id: String,
}

/// `POST /api/providers/device` — OAuth *device* flow, streamed over SSE.
///
/// Emits a `code` event (user code + verification URL) immediately, then blocks
/// while polling the provider, and finally emits `done` or `error`. No local
/// callback server is needed, which is why this flow suits the browser.
async fn provider_device<A: API + 'static>(
    State(state): State<AppState<A>>,
    Json(body): Json<DeviceBody>,
) -> Response {
    let provider_id = ProviderId::from(body.id);

    let method = match find_auth_method(state.api.as_ref(), &provider_id, &["device"]).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let request = match state.api.init_provider_auth(provider_id.clone(), method).await {
        Ok(AuthContextRequest::DeviceCode(req)) => req,
        Ok(_) => return AppError::bad_request("unexpected auth flow for device").into_response(),
        Err(e) => return AppError::from(e).into_response(),
    };

    let code_event = json!({
        "type": "code",
        "user_code": request.user_code.as_str(),
        "verification_uri": request.verification_uri.to_string(),
        "verification_uri_complete": request.verification_uri_complete.as_ref().map(|u| u.to_string()),
        "expires_in": request.expires_in,
    });

    let api = state.api.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(SseEvent::default().json_data(&code_event).unwrap());
        let response = AuthContextResponse::device_code(request);
        let done = match api.complete_provider_auth(provider_id, response, Duration::from_secs(600)).await {
            Ok(()) => json!({ "type": "done", "ok": true }),
            Err(e) => json!({ "type": "error", "message": format!("{e:?}") }),
        };
        yield Ok(SseEvent::default().json_data(&done).unwrap());
    };
    Sse::new(stream).into_response()
}

/// `DELETE /api/providers/{id}` — removes stored credentials (logout).
async fn remove_provider<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.api.remove_provider(&ProviderId::from(id)).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// MCP servers (the path to GitHub and other tool integrations)
// ---------------------------------------------------------------------------

/// An MCP server row for the connections panel.
#[derive(Serialize)]
struct McpDto {
    name: String,
    url: Option<String>,
    kind: String,
    /// Live connection state: `connected`, `failed`, `configured`, `unknown`.
    status: String,
    /// Human detail: tool count when connected, or the error when failed.
    detail: Option<String>,
}

/// `GET /api/mcp` — configured MCP servers with their *live* connection state.
///
/// Cross-references the config against the currently loaded tools so the UI can
/// show whether a server actually connected (and with how many tools) or failed
/// with an error — real feedback rather than just "configured".
async fn list_mcp<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<McpDto>>, AppError> {
    let config = state.api.read_mcp_config(None).await?;
    let tools = state.api.get_tools().await.ok();

    let mut out = Vec::new();
    for (name, server) in config.mcp_servers.iter() {
        let (status, detail) = match &tools {
            Some(t) if t.mcp.get_servers().contains_key(name) => (
                "connected",
                Some(format!("{} tools", t.mcp.get_servers()[name].len())),
            ),
            Some(t) if t.mcp.get_failures().contains_key(name) => {
                ("failed", Some(t.mcp.get_failures()[name].clone()))
            }
            Some(_) => ("configured", None),
            None => ("unknown", None),
        };
        let (url, kind) = match server {
            McpServerConfig::Http(http) => (Some(http.url.clone()), "http"),
            McpServerConfig::Stdio(_) => (None, "stdio"),
        };
        out.push(McpDto {
            name: name.to_string(),
            url,
            kind: kind.to_string(),
            status: status.to_string(),
            detail,
        });
    }
    Ok(Json(out))
}

/// Body for `POST /api/mcp`. Supports two server kinds:
/// - HTTP: give a `url` (+ optional `token` for a Bearer header).
/// - stdio: give a `command` (+ `args`, `env`) to launch a local MCP process,
///   e.g. `mcp-atlassian` for Jira.
#[derive(Deserialize)]
struct AddMcpBody {
    name: String,
    #[serde(default)]
    url: Option<String>,
    /// Optional token for HTTP servers. Configures an `Authorization: Bearer`
    /// header with OAuth disabled — a reliable way around endpoints that don't
    /// support OAuth dynamic client registration.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// `POST /api/mcp` — adds an HTTP or stdio MCP server to the user-scoped config.
async fn add_mcp<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<AddMcpBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }

    let command = body.command.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let url = body.url.as_deref().map(str::trim).filter(|u| !u.is_empty());

    let server = if let Some(command) = command {
        // Local process (stdio) — e.g. `mcp-atlassian` bridging Jira REST to MCP.
        let mut stdio = McpStdioServer::default();
        stdio.command = command.to_string();
        stdio.args = body.args.clone();
        stdio.env = body.env.clone().into_iter().collect();
        McpServerConfig::Stdio(stdio)
    } else if let Some(url) = url {
        let mut http = McpHttpServer::default();
        http.url = url.to_string();
        if let Some(token) = body.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            http.headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            http.oauth = McpOAuthSetting::Disabled;
        }
        McpServerConfig::Http(http)
    } else {
        return Err(AppError::bad_request("provide a url (HTTP) or a command (stdio)"));
    };

    let scope = Scope::User;
    // Serialize the read-modify-write against other config mutations.
    let _guard = state.config_lock.lock().await;
    let mut config = state.api.read_mcp_config(Some(&scope)).await.unwrap_or_default();
    config
        .mcp_servers
        .insert(ServerName::from(body.name.trim().to_string()), server);
    state.api.write_mcp_config(&scope, &config).await?;
    drop(_guard);
    // Reconnect so the change takes effect and status reflects reality.
    let _ = state.api.reload_mcp().await;
    Ok(Json(json!({ "ok": true })))
}

/// `DELETE /api/mcp/{name}` — removes a server from the user-scoped config.
async fn delete_mcp<A: API>(
    State(state): State<AppState<A>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let scope = Scope::User;
    let _guard = state.config_lock.lock().await;
    let mut config = state.api.read_mcp_config(Some(&scope)).await.unwrap_or_default();
    config.mcp_servers.remove(&ServerName::from(name));
    state.api.write_mcp_config(&scope, &config).await?;
    drop(_guard);
    let _ = state.api.reload_mcp().await;
    Ok(Json(json!({ "ok": true })))
}

/// An inline image attached to a chat message.
#[derive(Deserialize)]
struct ImageInput {
    /// Base64 payload (without the `data:` prefix).
    base64: String,
    mime: String,
    #[serde(default)]
    name: Option<String>,
}

/// Request body for `POST /api/chat`.
#[derive(Deserialize)]
struct ChatBody {
    conversation_id: String,
    message: String,
    #[serde(default)]
    images: Vec<ImageInput>,
}

/// `POST /api/chat` — streams the agent's response as Server-Sent Events.
///
/// Each SSE `data:` payload is a JSON [`ChatEventDto`]. The stream ends after a
/// `complete` (or `error`) event.
async fn chat<A: API>(State(state): State<AppState<A>>, Json(body): Json<ChatBody>) -> Response {
    let conversation_id = match ConversationId::parse(&body.conversation_id) {
        Ok(id) => id,
        Err(err) => return AppError::from(err).into_response(),
    };

    let mut event = DomainEvent::new(body.message);
    if !body.images.is_empty() {
        let attachments = body
            .images
            .into_iter()
            .map(|img| Attachment {
                content: AttachmentContent::Image(Image::new_base64(img.base64, img.mime)),
                path: img.name.unwrap_or_else(|| "pasted-image".to_string()),
            })
            .collect::<Vec<_>>();
        event = event.attachments(attachments);
    }
    let request = ChatRequest::new(event, conversation_id);

    let stream = match state.api.chat(request).await {
        Ok(stream) => stream,
        Err(err) => return AppError::from(err).into_response(),
    };

    let sse = stream.map(|item| {
        let dto = match &item {
            Ok(response) => {
                // The orchestrator awaits this notifier after emitting
                // `ToolCallStart` before it runs the tool. The TUI signals it
                // once the tool header is rendered; we must do the same or every
                // tool-using turn deadlocks.
                if let ChatResponse::ToolCallStart { notifier, .. } = response {
                    notifier.notify_one();
                }
                ChatEventDto::from(response)
            }
            Err(err) => ChatEventDto::Error { message: format!("{err:?}") },
        };
        // `json_data` only fails if the value can't be serialized, which our
        // DTOs always can; fall back to a plain error event just in case.
        let event = SseEvent::default().json_data(&dto).unwrap_or_else(|_| {
            SseEvent::default().data("{\"type\":\"error\",\"message\":\"serialize failed\"}")
        });
        Ok::<_, Infallible>(event)
    });

    Sse::new(sse).into_response()
}

/// Error wrapper that renders as a JSON error response.
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn not_found(message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{:?}", err.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
