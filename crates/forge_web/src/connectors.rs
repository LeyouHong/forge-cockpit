//! Declarative connectors — a manifest-driven integration system.
//!
//! A *connector* is a YAML manifest that declares how to call a platform's HTTP
//! API: endpoint templates, request shape, and a declarative auth scheme. A
//! generic engine substitutes the user's config + call arguments into those
//! templates, applies auth, and dispatches the request. Adding a platform is
//! writing a manifest — no Rust code changes.
//!
//! This is milestone 1: the `http` protocol plus a test-call endpoint. The
//! agent-facing `call_connector` tool, remote manifest sync, and the
//! `browser`/`ssh` protocols come in later milestones.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path as AxPath, Query, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::board::{client, read_settings, write_settings};
use crate::{AppError, AppState};

// ---------------------------------------------------------------------------
// Manifest schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Connector {
    pub id: String,
    pub name: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Base URL template, e.g. `https://gitlab.com/api/v4` or `{host}`.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: Option<Auth>,
    /// Default headers applied to every tool (e.g. an `Accept` header). Tool-level
    /// headers are added on top.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// User-supplied config fields (host, token, …), referenced as `{id}`.
    #[serde(default)]
    pub config: Vec<ConfigField>,
    #[serde(default)]
    pub tools: Vec<Tool>,
}

fn default_protocol() -> String {
    "http".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Auth {
    /// `none` | `bearer` | `header` | `query` | `basic` | `oauth`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub token: Option<String>,
    /// Header name for `kind: header`.
    #[serde(default)]
    pub header: Option<String>,
    /// Query-param name for `kind: query`.
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,

    // ----- oauth (kind: oauth) -----
    /// OAuth flow: `device` or `code` (authorization code + PKCE).
    #[serde(default)]
    pub flow: Option<String>,
    /// OAuth client id (may template `{field}` from config, e.g. a registered app id).
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth client secret for confidential clients (Atlassian/Sentry). Public
    /// clients (Azure desktop + PKCE) omit it. May template a secret config field.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Device-flow endpoint (flow: device).
    #[serde(default)]
    pub device_authorize_url: Option<String>,
    /// Authorization endpoint (flow: code).
    #[serde(default)]
    pub authorize_url: Option<String>,
    /// Extra static params for the authorize URL (e.g. `audience` for Atlassian).
    #[serde(default)]
    pub authorize_params: BTreeMap<String, String>,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// How the obtained access token is presented on requests: `bearer` (default)
    /// or `header` (with `header:` naming the header).
    #[serde(rename = "use", default)]
    pub use_as: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ConfigField {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_method")]
    pub method: String,
    /// Path template, appended to `base_url`. May contain `{param}`/`{config}`.
    pub path: String,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Optional JSON body; string leaves are templated.
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub params: Vec<Param>,
}

fn default_method() -> String {
    "GET".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Param {
    pub id: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Manifest loading (bundled for now; remote sync is a later milestone)
// ---------------------------------------------------------------------------

const BUNDLED: &[&str] = &[
    include_str!("../connectors/demo.yaml"),
    include_str!("../connectors/gitlab.yaml"),
    include_str!("../connectors/github.yaml"),
    include_str!("../connectors/jira.yaml"),
    include_str!("../connectors/sentry.yaml"),
    include_str!("../connectors/teams.yaml"),
];

/// Local cache dir for manifests synced from a remote source.
fn cache_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".forge-web-connectors"))
}

/// All manifests: the bundled examples plus any synced into the cache dir. A
/// synced manifest with the same id overrides a bundled one.
fn manifests() -> Vec<Connector> {
    let mut by_id: BTreeMap<String, Connector> = BUNDLED
        .iter()
        .filter_map(|y| serde_yml::from_str::<Connector>(y).ok())
        .map(|c| (c.id.clone(), c))
        .collect();
    if let Some(dir) = cache_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml")
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(c) = serde_yml::from_str::<Connector>(&text)
            {
                by_id.insert(c.id.clone(), c);
            }
        }
    }
    by_id.into_values().collect()
}

/// The configured remote manifest source (a base URL), if any.
fn connector_source() -> Option<String> {
    read_settings()
        .get("connector_source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn manifest(id: &str) -> Option<Connector> {
    manifests().into_iter().find(|c| c.id == id)
}

/// Per-connector config the user has saved, `{ "<field>": "<value>" }`. Secret
/// values are stored encrypted; decrypting here is a no-op on plaintext.
fn connector_config(id: &str) -> BTreeMap<String, String> {
    read_settings()
        .get("connectors")
        .and_then(|c| c.get(id))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), crate::secret::decrypt(s))))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// OAuth token storage (per connector, encrypted) + refresh
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn oauth_store_read(id: &str) -> Option<Value> {
    read_settings().get("oauth_tokens").and_then(|t| t.get(id)).cloned()
}

fn oauth_store_write(id: &str, access: &str, refresh: Option<&str>, expires_in: Option<u64>) {
    let mut s = read_settings();
    if !s.get("oauth_tokens").map(Value::is_object).unwrap_or(false) {
        s["oauth_tokens"] = json!({});
    }
    let mut entry = json!({ "access_token": crate::secret::encrypt(access) });
    if let Some(r) = refresh {
        entry["refresh_token"] = json!(crate::secret::encrypt(r));
    }
    if let Some(e) = expires_in {
        entry["expires_at"] = json!(now_secs() + e);
    }
    s["oauth_tokens"][id] = entry;
    write_settings(&s);
}

fn oauth_clear(id: &str) {
    let mut s = read_settings();
    if let Some(Value::Object(m)) = s.get_mut("oauth_tokens") {
        m.remove(id);
    }
    write_settings(&s);
}

fn oauth_connected(id: &str) -> bool {
    oauth_store_read(id)
        .and_then(|t| t.get("access_token").cloned())
        .is_some()
}

/// A usable access token for the connector, refreshing if expired and a refresh
/// token is available.
async fn oauth_access_token(connector: &Connector) -> Option<String> {
    let stored = oauth_store_read(&connector.id)?;
    let access = stored
        .get("access_token")
        .and_then(Value::as_str)
        .map(crate::secret::decrypt)?;
    let expires_at = stored.get("expires_at").and_then(Value::as_u64);
    if expires_at.map(|e| e > now_secs() + 30).unwrap_or(true) {
        return Some(access); // not expired (or no expiry)
    }
    // Expired → refresh.
    let refresh = stored
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(crate::secret::decrypt)?;
    let auth = connector.auth.as_ref()?;
    let token_url = auth.token_url.as_deref()?;
    let vars = connector_config(&connector.id);
    let client_id = render(auth.client_id.as_deref().unwrap_or(""), &vars);
    let resp: Value = client()
        .post(token_url)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let new_access = resp.get("access_token").and_then(Value::as_str)?.to_string();
    oauth_store_write(
        &connector.id,
        &new_access,
        resp.get("refresh_token").and_then(Value::as_str),
        resp.get("expires_in").and_then(Value::as_u64),
    );
    Some(new_access)
}

// Authorization-code (PKCE) flows in progress, keyed by the `state` value. The
// browser hits /oauth/callback (unauthenticated) with the code + state; we match
// it here to finish the exchange server-side.
struct PendingOAuth {
    connector_id: String,
    verifier: String,
    token_url: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    created: u64,
}

static PENDING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, PendingOAuth>>> =
    std::sync::LazyLock::new(Default::default);

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// ---------------------------------------------------------------------------
// Template rendering — `{key}` is replaced from the vars map; unknown keys
// render to empty so optional query params simply drop out.
// ---------------------------------------------------------------------------

fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        if let Some(close) = rest.find('}') {
            let key = &rest[..close];
            out.push_str(vars.get(key).map(String::as_str).unwrap_or(""));
            rest = &rest[close + 1..];
        } else {
            out.push('{');
            break;
        }
    }
    out.push_str(rest);
    out
}

fn render_json(v: &Value, vars: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(render(s, vars)),
        Value::Array(a) => Value::Array(a.iter().map(|x| render_json(x, vars)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), render_json(x, vars))).collect())
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Dispatch engine
// ---------------------------------------------------------------------------

async fn dispatch(connector: &Connector, tool: &Tool, args: &Value) -> Result<Value, AppError> {
    if connector.protocol != "http" {
        return Err(AppError::bad_request(format!(
            "connector '{}' uses protocol '{}', which this milestone doesn't run yet",
            connector.id, connector.protocol
        )));
    }

    // vars = saved config + call args (args win on conflict).
    let mut vars = connector_config(&connector.id);
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            vars.insert(k.clone(), s);
        }
    }

    // Required config / params must be present and non-empty.
    for f in &connector.config {
        if f.required && vars.get(&f.id).map(|s| s.is_empty()).unwrap_or(true) {
            return Err(AppError::bad_request(format!(
                "connector '{}' is missing required config '{}'",
                connector.id, f.id
            )));
        }
    }
    for p in &tool.params {
        if p.required && vars.get(&p.id).map(|s| s.is_empty()).unwrap_or(true) {
            return Err(AppError::bad_request(format!("missing required argument '{}'", p.id)));
        }
    }

    let base = render(connector.base_url.as_deref().unwrap_or(""), &vars);
    let path = render(&tool.path, &vars);
    let url = format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'));

    let method = reqwest::Method::from_bytes(tool.method.to_uppercase().as_bytes())
        .map_err(|_| AppError::bad_request(format!("bad method '{}'", tool.method)))?;
    let mut req = client().request(method, &url).header("Accept", "application/json");

    for (k, v) in &tool.query {
        let val = render(v, &vars);
        if !val.is_empty() {
            req = req.query(&[(k.as_str(), val)]);
        }
    }
    for (k, v) in &connector.headers {
        req = req.header(k, render(v, &vars));
    }
    for (k, v) in &tool.headers {
        req = req.header(k, render(v, &vars));
    }

    // Declarative auth.
    if let Some(auth) = &connector.auth {
        let token = render(auth.token.as_deref().unwrap_or(""), &vars);
        match auth.kind.as_str() {
            "none" => {}
            "bearer" => req = req.header("Authorization", format!("Bearer {token}")),
            "header" => {
                let name = auth.header.clone().unwrap_or_else(|| "Authorization".to_string());
                req = req.header(name, token);
            }
            "query" => {
                let name = auth.param.clone().unwrap_or_else(|| "token".to_string());
                req = req.query(&[(name.as_str(), token)]);
            }
            "basic" => {
                let user = render(auth.username.as_deref().unwrap_or(""), &vars);
                let pass = render(auth.password.as_deref().unwrap_or(""), &vars);
                req = req.basic_auth(user, Some(pass));
            }
            "oauth" => {
                let token = oauth_access_token(connector).await.ok_or_else(|| {
                    AppError::bad_request(format!(
                        "connector '{}' is not connected — run its OAuth flow first",
                        connector.id
                    ))
                })?;
                match auth.use_as.as_deref().unwrap_or("bearer") {
                    "header" => {
                        let name =
                            auth.header.clone().unwrap_or_else(|| "Authorization".to_string());
                        req = req.header(name, token);
                    }
                    _ => req = req.header("Authorization", format!("Bearer {token}")),
                }
            }
            other => {
                return Err(AppError::bad_request(format!("unsupported auth type '{other}'")));
            }
        }
    }

    if let Some(body) = &tool.body {
        req = req.json(&render_json(body, &vars));
    }

    let resp = req.send().await.map_err(|e| AppError::bad_request(format!("request failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    // Return parsed JSON when possible, else raw text — plus the status so the
    // caller (and later the agent) can see non-2xx responses.
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok(json!({ "status": status, "ok": (200..300).contains(&status), "data": data }))
}

// ---------------------------------------------------------------------------
// HTTP endpoints
// ---------------------------------------------------------------------------

/// The connector catalog as JSON: every manifest, whether it's configured, and
/// the tools it exposes. Shared by the HTTP endpoint and the MCP server.
pub(crate) fn catalog_json() -> Value {
    let out: Vec<Value> = manifests()
        .into_iter()
        .map(|c| {
            let cfg = connector_config(&c.id);
            let auth_type = c.auth.as_ref().map(|a| a.kind.clone());
            let is_oauth = auth_type.as_deref() == Some("oauth");
            let connected = if is_oauth { oauth_connected(&c.id) } else { false };
            let config_ok = c
                .config
                .iter()
                .filter(|f| f.required)
                .all(|f| cfg.get(&f.id).map(|s| !s.is_empty()).unwrap_or(false));
            // An oauth connector also needs a token; others just need their config.
            let configured = config_ok && (!is_oauth || connected);
            // Echo back saved *non-secret* values so the form can prefill;
            // secrets are never returned.
            let values: BTreeMap<&str, &str> = c
                .config
                .iter()
                .filter(|f| !f.secret)
                .filter_map(|f| cfg.get(&f.id).map(|v| (f.id.as_str(), v.as_str())))
                .collect();
            json!({
                "id": c.id,
                "name": c.name,
                "description": c.description,
                "protocol": c.protocol,
                "configured": configured,
                "auth_type": auth_type,
                "oauth_flow": c.auth.as_ref().and_then(|a| a.flow.clone()),
                "oauth_connected": connected,
                "config": c.config,
                "values": values,
                "tools": c.tools.iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "method": t.method,
                    "params": t.params,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "connectors": out })
}

/// Looks up a connector + tool by name and dispatches. Shared by the HTTP
/// endpoint and the MCP server.
pub(crate) async fn dispatch_by_name(
    connector_id: &str,
    tool: &str,
    args: &Value,
) -> Result<Value, AppError> {
    let connector = manifest(connector_id).ok_or_else(|| AppError::not_found("no such connector"))?;
    let tool = connector.tools.iter().find(|t| t.name == tool).ok_or_else(|| {
        AppError::not_found(format!("connector '{connector_id}' has no tool '{tool}'"))
    })?;
    dispatch(&connector, tool, args).await
}

use forge_api::API;

/// GET /api/connectors — the catalog + whether each is configured.
pub(crate) async fn list_connectors<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    Json(catalog_json())
}

#[derive(Deserialize)]
pub(crate) struct ConfigBody {
    values: BTreeMap<String, String>,
}

/// PUT /api/connectors/{id}/config — save the user's config values.
pub(crate) async fn set_config<A: API>(
    State(_): State<AppState<A>>,
    AxPath(id): AxPath<String>,
    Json(body): Json<ConfigBody>,
) -> Result<Json<Value>, AppError> {
    let connector = manifest(&id).ok_or_else(|| AppError::not_found("no such connector"))?;
    let allowed: std::collections::HashSet<&str> =
        connector.config.iter().map(|f| f.id.as_str()).collect();
    let secret_fields: std::collections::HashSet<&str> =
        connector.config.iter().filter(|f| f.secret).map(|f| f.id.as_str()).collect();
    let mut s = read_settings();
    if !s.get("connectors").map(Value::is_object).unwrap_or(false) {
        s["connectors"] = json!({});
    }
    let entry = s["connectors"][&id].as_object().cloned().unwrap_or_default();
    let mut entry = Value::Object(entry);
    for (k, v) in body.values {
        if allowed.contains(k.as_str()) {
            // Secret fields are encrypted at rest; others stay plaintext.
            let stored = if secret_fields.contains(k.as_str()) {
                crate::secret::encrypt(&v)
            } else {
                v
            };
            entry[k] = json!(stored);
        }
    }
    s["connectors"][&id] = entry;
    write_settings(&s);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct CallBody {
    tool: String,
    #[serde(default)]
    args: Value,
}

/// POST /api/connectors/{id}/call — run one tool of a connector.
pub(crate) async fn call_connector<A: API>(
    State(_): State<AppState<A>>,
    AxPath(id): AxPath<String>,
    Json(body): Json<CallBody>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(dispatch_by_name(&id, &body.tool, &body.args).await?))
}

// ---------------------------------------------------------------------------
// Remote manifest sync — pull manifests from a configured source into the cache
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct SourceBody {
    url: Option<String>,
}

/// GET /api/connectors/source — the configured remote source (if any).
pub(crate) async fn get_source<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    Json(json!({ "url": connector_source() }))
}

/// PUT /api/connectors/source — set (or clear) the remote source base URL.
pub(crate) async fn set_source<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<SourceBody>,
) -> Json<Value> {
    let mut s = read_settings();
    match body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => {
            s["connector_source"] = json!(u);
        }
        None => {
            if let Value::Object(m) = &mut s {
                m.remove("connector_source");
            }
        }
    }
    write_settings(&s);
    Json(json!({ "ok": true }))
}

/// Only allow a plain slug as a cache filename (defends against path traversal
/// from a manifest's declared id).
fn safe_slug(id: &str) -> Option<String> {
    if !id.is_empty()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(id.to_string())
    } else {
        None
    }
}

/// POST /api/connectors/sync — fetch `<source>/index.json` (a JSON array of
/// connector ids, or `{ "connectors": [...] }`), download each `<source>/<id>.yaml`,
/// validate it parses, and write it into the local cache dir.
pub(crate) async fn sync<A: API>(State(_): State<AppState<A>>) -> Result<Json<Value>, AppError> {
    let base = connector_source()
        .ok_or_else(|| AppError::bad_request("no connector source configured"))?;
    let base = base.trim_end_matches('/').to_string();
    let cl = client();
    let index: Value = cl
        .get(format!("{base}/index.json"))
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("fetch index.json: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::bad_request(format!("parse index.json: {e}")))?;
    let ids: Vec<String> = index
        .as_array()
        .or_else(|| index.get("connectors").and_then(Value::as_array))
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Err(AppError::bad_request("index.json listed no connectors"));
    }
    let dir = cache_dir().ok_or_else(|| AppError::bad_request("no HOME for cache dir"))?;
    std::fs::create_dir_all(&dir).map_err(|e| AppError::bad_request(format!("cache dir: {e}")))?;

    let mut synced = Vec::new();
    let mut errors = Vec::new();
    for id in ids {
        let text = match cl.get(format!("{base}/{id}.yaml")).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(e) => {
                errors.push(format!("{id}: fetch failed: {e}"));
                continue;
            }
        };
        let connector = match serde_yml::from_str::<Connector>(&text) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{id}: invalid manifest: {e}"));
                continue;
            }
        };
        match safe_slug(&connector.id) {
            Some(slug) if std::fs::write(dir.join(format!("{slug}.yaml")), &text).is_ok() => {
                synced.push(connector.id);
            }
            Some(_) => errors.push(format!("{id}: write failed")),
            None => errors.push(format!("{id}: unsafe connector id '{}'", connector.id)),
        }
    }
    Ok(Json(json!({ "synced": synced, "errors": errors })))
}

// ---------------------------------------------------------------------------
// OAuth device flow — POST /api/connectors/{id}/oauth/start (SSE), and
// DELETE /api/connectors/{id}/oauth to disconnect.
// ---------------------------------------------------------------------------

fn sse(v: &Value) -> Result<SseEvent, std::convert::Infallible> {
    Ok(SseEvent::default().json_data(v).unwrap_or_else(|_| SseEvent::default().data("{}")))
}

/// POST /api/connectors/{id}/oauth/start — starts the connector's OAuth flow.
/// For `device`, streams the user code + result over SSE. For `code`, returns
/// `{ authorize_url }` for the browser to open; the exchange finishes at the
/// loopback `/oauth/callback`.
pub(crate) async fn oauth_start<A: API + 'static>(
    State(_): State<AppState<A>>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let connector = match manifest(&id) {
        Some(c) => c,
        None => return AppError::not_found("no such connector").into_response(),
    };
    let auth = match &connector.auth {
        Some(a) if a.kind == "oauth" => a.clone(),
        _ => return AppError::bad_request("connector is not oauth").into_response(),
    };
    let vars = connector_config(&id);
    let client_id = render(auth.client_id.as_deref().unwrap_or(""), &vars);
    if client_id.is_empty() {
        return AppError::bad_request("client_id is not configured").into_response();
    }

    // ----- authorization code + PKCE (loopback redirect) -----
    if auth.flow.as_deref() == Some("code") {
        let (Some(authorize_url), Some(token_url)) =
            (auth.authorize_url.clone(), auth.token_url.clone())
        else {
            return AppError::bad_request("manifest lacks authorize_url / token_url")
                .into_response();
        };
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1");
        // For a desktop shell this becomes a custom scheme (forge://oauth/callback);
        // on the loopback web server it's the server's own /oauth/callback.
        let redirect_uri = format!("http://{host}/oauth/callback");
        let verifier = crate::secret::random_urlsafe(48);
        let state = crate::secret::random_urlsafe(24);
        let mut params: Vec<(String, String)> = vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), client_id.clone()),
            ("redirect_uri".into(), redirect_uri.clone()),
            ("scope".into(), auth.scopes.join(" ")),
            ("state".into(), state.clone()),
            ("code_challenge".into(), pkce_challenge(&verifier)),
            ("code_challenge_method".into(), "S256".into()),
        ];
        for (k, v) in &auth.authorize_params {
            params.push((k.clone(), render(v, &vars)));
        }
        let url = match reqwest::Url::parse(&authorize_url) {
            Ok(mut u) => {
                for (k, v) in &params {
                    u.query_pairs_mut().append_pair(k, v);
                }
                u.to_string()
            }
            Err(_) => return AppError::bad_request("bad authorize_url").into_response(),
        };
        {
            let mut pending = PENDING.lock().unwrap();
            pending.retain(|_, v| now_secs().saturating_sub(v.created) < 900);
            pending.insert(
                state,
                PendingOAuth {
                    connector_id: id.clone(),
                    verifier,
                    token_url,
                    client_id,
                    client_secret: render(auth.client_secret.as_deref().unwrap_or(""), &vars),
                    redirect_uri,
                    created: now_secs(),
                },
            );
        }
        return Json(json!({ "flow": "code", "authorize_url": url })).into_response();
    }

    // ----- device flow -----
    if auth.flow.as_deref() != Some("device") {
        return AppError::bad_request("unsupported oauth flow").into_response();
    }
    let (Some(device_url), Some(token_url)) =
        (auth.device_authorize_url.clone(), auth.token_url.clone())
    else {
        return AppError::bad_request("manifest lacks device_authorize_url / token_url")
            .into_response();
    };
    let scope = auth.scopes.join(" ");

    let stream = async_stream::stream! {
        let cl = client();
        let dev: Value = match cl.post(&device_url).header("Accept", "application/json")
            .form(&[("client_id", client_id.as_str()), ("scope", scope.as_str())])
            .send().await
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(e) => { yield sse(&json!({ "type": "error", "message": format!("device code request failed: {e}") })); return; }
        };
        let device_code = dev.get("device_code").and_then(Value::as_str).unwrap_or("").to_string();
        if device_code.is_empty() {
            let m = dev.get("error_description").or_else(|| dev.get("error")).and_then(Value::as_str).unwrap_or("no device_code returned");
            yield sse(&json!({ "type": "error", "message": m }));
            return;
        }
        let mut interval = dev.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1);
        let expires_in = dev.get("expires_in").and_then(Value::as_u64).unwrap_or(600);
        yield sse(&json!({
            "type": "code",
            "user_code": dev.get("user_code").and_then(Value::as_str).unwrap_or(""),
            "verification_uri": dev.get("verification_uri").and_then(Value::as_str).unwrap_or(""),
            "expires_in": expires_in,
        }));

        let deadline = now_secs() + expires_in;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            if now_secs() > deadline {
                yield sse(&json!({ "type": "error", "message": "device code expired" }));
                return;
            }
            let tok: Value = match cl.post(&token_url).header("Accept", "application/json")
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("device_code", device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]).send().await
            {
                Ok(r) => r.json().await.unwrap_or_default(),
                Err(e) => { yield sse(&json!({ "type": "error", "message": format!("token poll failed: {e}") })); return; }
            };
            if let Some(access) = tok.get("access_token").and_then(Value::as_str) {
                oauth_store_write(
                    &id,
                    access,
                    tok.get("refresh_token").and_then(Value::as_str),
                    tok.get("expires_in").and_then(Value::as_u64),
                );
                yield sse(&json!({ "type": "done", "ok": true }));
                return;
            }
            match tok.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => continue,
                Some("slow_down") => { interval += 5; continue; }
                Some(other) => { yield sse(&json!({ "type": "error", "message": other })); return; }
                None => { yield sse(&json!({ "type": "error", "message": "unexpected token response" })); return; }
            }
        }
    };
    Sse::new(stream).into_response()
}

/// DELETE /api/connectors/{id}/oauth — forget the stored OAuth token.
pub(crate) async fn oauth_disconnect<A: API>(
    State(_): State<AppState<A>>,
    AxPath(id): AxPath<String>,
) -> Json<Value> {
    oauth_clear(&id);
    Json(json!({ "ok": true }))
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn done_page(msg: &str) -> Response {
    Html(format!(
        "<!doctype html><meta charset=utf-8><body style=\"font:15px -apple-system,system-ui,sans-serif;padding:48px;color:#222\">\
         {msg}<p style=\"color:#888;margin-top:14px\">You can close this tab and return to Forge.</p></body>"
    ))
    .into_response()
}

/// GET /oauth/callback — the loopback redirect target for the code flow. This is
/// public (the browser carries no bearer token); the `state` match is the CSRF
/// guard. Finishes the code→token exchange server-side and stores the token.
pub(crate) async fn oauth_callback<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if let Some(err) = q.error {
        return done_page(&format!("<h3>Authorization failed</h3><p>{}</p>", html_escape(&err)));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return done_page("<h3>Missing code or state</h3>");
    };
    let Some(p) = PENDING.lock().unwrap().remove(&state) else {
        return done_page("<h3>Unknown or expired authorization request</h3>");
    };
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", p.redirect_uri.as_str()),
        ("client_id", p.client_id.as_str()),
        ("code_verifier", p.verifier.as_str()),
    ];
    if !p.client_secret.is_empty() {
        form.push(("client_secret", p.client_secret.as_str()));
    }
    let resp: Value = match client()
        .post(&p.token_url)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            return done_page(&format!("<h3>Token exchange failed</h3><p>{}</p>", html_escape(&e.to_string())));
        }
    };
    match resp.get("access_token").and_then(Value::as_str) {
        Some(access) => {
            oauth_store_write(
                &p.connector_id,
                access,
                resp.get("refresh_token").and_then(Value::as_str),
                resp.get("expires_in").and_then(Value::as_u64),
            );
            done_page("<h3>✓ Connected</h3>")
        }
        None => {
            let m = resp
                .get("error_description")
                .or_else(|| resp.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no access_token in response");
            done_page(&format!("<h3>Token exchange failed</h3><p>{}</p>", html_escape(m)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifests_parse() {
        let all = manifests();
        for id in ["demo", "gitlab", "github", "jira", "sentry", "teams"] {
            assert!(all.iter().any(|c| c.id == id), "bundled manifest '{id}' should deserialize");
        }
    }

    #[test]
    fn render_substitutes_and_drops_unknown() {
        let mut vars = BTreeMap::new();
        vars.insert("host".to_string(), "https://x".to_string());
        assert_eq!(render("{host}/api", &vars), "https://x/api");
        // Unknown placeholder renders empty (optional query params drop out).
        assert_eq!(render("a={user}", &vars), "a=");
    }
}
