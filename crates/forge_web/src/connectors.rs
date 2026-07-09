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
use axum::extract::{Path as AxPath, State};
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
    /// `none` | `bearer` | `header` | `query` | `basic`.
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
];

fn manifests() -> Vec<Connector> {
    BUNDLED
        .iter()
        .filter_map(|y| serde_yml::from_str::<Connector>(y).ok())
        .collect()
}

fn manifest(id: &str) -> Option<Connector> {
    manifests().into_iter().find(|c| c.id == id)
}

/// Per-connector config the user has saved, `{ "<field>": "<value>" }`.
fn connector_config(id: &str) -> BTreeMap<String, String> {
    read_settings()
        .get("connectors")
        .and_then(|c| c.get(id))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
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

/// GET /api/connectors — the catalog + whether each is configured.
pub(crate) async fn list_connectors<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    let out: Vec<Value> = manifests()
        .into_iter()
        .map(|c| {
            let cfg = connector_config(&c.id);
            let configured = c
                .config
                .iter()
                .filter(|f| f.required)
                .all(|f| cfg.get(&f.id).map(|s| !s.is_empty()).unwrap_or(false));
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
    Json(json!({ "connectors": out }))
}

use forge_api::API;

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
    let mut s = read_settings();
    if !s.get("connectors").map(Value::is_object).unwrap_or(false) {
        s["connectors"] = json!({});
    }
    let entry = s["connectors"][&id].as_object().cloned().unwrap_or_default();
    let mut entry = Value::Object(entry);
    for (k, v) in body.values {
        if allowed.contains(k.as_str()) {
            entry[k] = json!(v);
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
    let connector = manifest(&id).ok_or_else(|| AppError::not_found("no such connector"))?;
    let tool = connector
        .tools
        .iter()
        .find(|t| t.name == body.tool)
        .ok_or_else(|| AppError::not_found(format!("connector '{id}' has no tool '{}'", body.tool)))?;
    let result = dispatch(&connector, tool, &body.args).await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifests_parse() {
        let all = manifests();
        assert_eq!(all.len(), 2, "both bundled manifests should deserialize");
        assert!(all.iter().any(|c| c.id == "demo"));
        assert!(all.iter().any(|c| c.id == "gitlab"));
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
