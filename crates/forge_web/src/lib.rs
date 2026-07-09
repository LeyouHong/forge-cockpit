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

mod board;
mod connectors;
mod connectors_mcp;
mod dto;
mod live;
mod secret;

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
use axum::routing::{delete, get, post, put};
use forge_api::API;
use forge_domain::{
    AgentId, AuthContextRequest, AuthContextResponse, AuthMethod, ConfigOperation, Context,
    ContextMessage, Conversation, ConversationId, McpHttpServer, McpOAuthSetting, McpServerConfig,
    McpStdioServer, ModelId, ProviderId, Role, Scope, ServerName,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dto::MessageDto;

/// Shared application state handed to every request handler.
pub(crate) struct AppState<A> {
    pub(crate) api: Arc<A>,
    /// Per-run bearer token that gates the UI and API.
    pub(crate) token: Arc<String>,
    /// Serializes MCP config read-modify-write so concurrent operations can't
    /// corrupt the config file.
    pub(crate) config_lock: Arc<tokio::sync::Mutex<()>>,
    /// Resumable chat turns, keyed by conversation id.
    pub(crate) turns: live::TurnRegistry,
    /// Recently finished turns for the tasks panel (in-memory, per run).
    pub(crate) history: live::TaskHistory,
}

// Manual `Clone` so we don't require `A: Clone` (only the `Arc`s are cloned).
impl<A> Clone for AppState<A> {
    fn clone(&self) -> Self {
        Self {
            api: self.api.clone(),
            token: self.token.clone(),
            config_lock: self.config_lock.clone(),
            turns: self.turns.clone(),
            history: self.history.clone(),
        }
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
    let state = AppState {
        api,
        token: Arc::new(token),
        config_lock: Arc::new(tokio::sync::Mutex::new(())),
        turns: Default::default(),
        history: Default::default(),
    };

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
        .route("/api/conversations/{id}/export", get(export_conversation::<A>))
        .route("/api/commands", get(list_commands::<A>))
        .route("/api/skills", get(list_skills::<A>))
        .route("/api/agents", get(list_agents::<A>).post(set_agent::<A>))
        .route("/api/models", get(list_models::<A>).post(set_model::<A>))
        .route("/api/providers", get(list_providers::<A>))
        .route("/api/providers/apikey", post(provider_apikey::<A>))
        .route("/api/providers/device", post(provider_device::<A>))
        .route("/api/providers/{id}", delete(remove_provider::<A>))
        .route("/api/mcp", get(list_mcp::<A>).post(add_mcp::<A>))
        .route("/api/mcp/{name}", delete(delete_mcp::<A>))
        .route("/api/chat", post(live::start_turn::<A>))
        .route("/api/chat/{conv}/live", get(live::turn_live::<A>))
        .route("/api/chat/{conv}/stop", post(live::turn_stop::<A>))
        .route("/api/tasks", get(live::list_tasks::<A>))
        .route("/api/conversations/{id}/usage", get(live::get_usage::<A>))
        .route("/api/board/platforms", get(board::platforms::<A>))
        .route("/api/board/github", get(board::github_board::<A>))
        .route("/api/board/gha", get(board::gha_board::<A>))
        .route("/api/board/jira", get(board::jira_board::<A>))
        .route("/api/board/sentry", get(board::sentry_board::<A>))
        .route("/api/board/gcal", get(board::gcal_board::<A>))
        .route("/api/gcal", get(board::get_gcal::<A>).put(board::set_gcal::<A>))
        .route("/api/todos", get(board::list_todos::<A>).post(board::add_todo::<A>))
        .route(
            "/api/todos/{id}",
            put(board::update_todo::<A>).delete(board::delete_todo::<A>),
        )
        .route("/api/pipelines", get(board::running_pipelines::<A>))
        .route("/api/connectors", get(connectors::list_connectors::<A>))
        .route(
            "/api/connectors/source",
            get(connectors::get_source::<A>).put(connectors::set_source::<A>),
        )
        .route("/api/connectors/sync", post(connectors::sync::<A>))
        .route("/api/connectors/{id}/config", put(connectors::set_config::<A>))
        .route("/api/connectors/{id}/call", post(connectors::call_connector::<A>))
        .route_layer(from_fn_with_state(state.clone(), auth::<A>));

    // The connector engine, exposed to the agent as a Streamable-HTTP MCP
    // endpoint at `/mcp` (auth-gated with the same bearer token). It's
    // auto-registered into the MCP config below so the agent gets the tools.
    let mcp_router = Router::new()
        .nest_service("/mcp", connectors_mcp::service())
        .layer(from_fn_with_state(state.clone(), auth::<A>));

    let app = Router::new()
        .route("/", get(index::<A>))
        .merge(api_routes)
        .merge(mcp_router)
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

    // Register the `/mcp` endpoint into Forge's MCP config so the agent picks up
    // the connector tools. The url + token are per-run, so we overwrite the entry
    // each startup once the server is accepting connections.
    {
        let state = state.clone();
        let mcp_url = format!("http://{local}/mcp");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            register_connectors_mcp(&state, &mcp_url).await;
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Registers (overwriting) the self-hosted connectors MCP endpoint in the
/// user-scoped MCP config, then reloads so the running agent picks it up.
async fn register_connectors_mcp<A: API>(state: &AppState<A>, url: &str) {
    let scope = Scope::User;
    let _guard = state.config_lock.lock().await;
    let mut config = state.api.read_mcp_config(Some(&scope)).await.unwrap_or_default();
    let mut http = McpHttpServer::default();
    http.url = url.to_string();
    http.headers
        .insert("Authorization".to_string(), format!("Bearer {}", state.token));
    http.oauth = McpOAuthSetting::Disabled;
    config
        .mcp_servers
        .insert(ServerName::from("connectors".to_string()), McpServerConfig::Http(http));
    if let Err(err) = state.api.write_mcp_config(&scope, &config).await {
        tracing::warn!("could not register connectors MCP server: {err}");
        return;
    }
    drop(_guard);
    let _ = state.api.reload_mcp().await;
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

/// Serves the single-page frontend, gated by the token.
///
/// The token arrives as a query parameter on first load; we then set it as an
/// HttpOnly cookie so a plain refresh (after the query string is stripped from
/// the address bar) still authorizes the shell. The token is also injected into
/// the page for the frontend's API calls, which authorize via a bearer header —
/// the cookie only re-serves this HTML and can't call the API on its own.
async fn index<A>(
    State(state): State<AppState<A>>,
    Query(query): Query<IndexQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let query_ok = query.token.as_deref() == Some(state.token.as_str());
    let want = format!("forge_token={}", state.token);
    let cookie_ok = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|c| c.split(';').any(|kv| kv.trim() == want))
        .unwrap_or(false);
    if !query_ok && !cookie_ok {
        return (
            StatusCode::UNAUTHORIZED,
            Html("<h1>Unauthorized</h1><p>Open the URL printed by <code>forge serve</code>, which includes the access token.</p>"),
        )
            .into_response();
    }
    let page = include_str!("index.html").replace("__FORGE_TOKEN__", &state.token);
    let mut resp = Html(page).into_response();
    if query_ok {
        if let Ok(cookie) = format!("{want}; Path=/; SameSite=Strict; HttpOnly").parse() {
            resp.headers_mut().insert(axum::http::header::SET_COOKIE, cookie);
        }
    }
    resp
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
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
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

/// `GET /api/conversations/{id}/export` — the conversation as a standalone HTML
/// document (Forge's own rendering), for download.
async fn export_conversation<A: API>(
    State(state): State<AppState<A>>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id = ConversationId::parse(&id)?;
    let conversation = state
        .api
        .conversation(&id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("conversation '{id}' not found")))?;
    let html = conversation.to_html();
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}

/// A command exposed to the command palette.
#[derive(Serialize)]
struct CommandDto {
    name: String,
    description: String,
    prompt: Option<String>,
}

/// `GET /api/commands` — custom commands (named prompt templates).
async fn list_commands<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<CommandDto>>, AppError> {
    let commands = state.api.get_commands().await?;
    Ok(Json(
        commands
            .into_iter()
            .map(|c| CommandDto { name: c.name, description: c.description, prompt: c.prompt })
            .collect(),
    ))
}

/// A skill exposed for discoverability.
#[derive(Serialize)]
struct SkillDto {
    name: String,
    description: String,
}

/// `GET /api/skills` — available skills (agent-invoked domain knowledge).
async fn list_skills<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Vec<SkillDto>>, AppError> {
    let skills = state.api.get_skills().await?;
    Ok(Json(
        skills
            .into_iter()
            .map(|s| SkillDto { name: s.name, description: s.description })
            .collect(),
    ))
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
    supports_image: bool,
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
            supports_image: m.input_modalities.contains(&forge_domain::InputModality::Image),
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

/// Error wrapper that renders as a JSON error response.
pub(crate) struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
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
