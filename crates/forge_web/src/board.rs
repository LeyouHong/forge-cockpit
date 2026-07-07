//! Read-only Kanban boards over the connected integrations.
//!
//! Rather than route through the agent, each board queries the platform's REST
//! API directly using the credentials the user already configured in their MCP
//! integrations (GitHub PAT, Jira URL/email/token, Sentry token). Everything is
//! read-only.

use std::path::Path;
use std::process::Command;

use axum::Json;
use axum::extract::State;
use base64::Engine;
use forge_api::API;
use forge_domain::McpServerConfig;
use serde_json::{Value, json};

use crate::{AppError, AppState};

/// Looks up a configured MCP server by name.
async fn server<A: API>(state: &AppState<A>, name: &str) -> Option<McpServerConfig> {
    let cfg = state.api.read_mcp_config(None).await.ok()?;
    cfg.mcp_servers
        .iter()
        .find(|(n, _)| n.to_string() == name)
        .map(|(_, s)| s.clone())
}

fn github_pat(server: &McpServerConfig) -> Option<String> {
    if let McpServerConfig::Http(http) = server {
        return http
            .headers
            .get("Authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
    }
    None
}

fn stdio_env(server: &McpServerConfig, key: &str) -> Option<String> {
    if let McpServerConfig::Stdio(s) = server {
        return s.env.get(key).cloned();
    }
    None
}

fn origin_slug(git_root: &Path) -> Option<(String, String)> {
    let url = String::from_utf8(
        Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(git_root)
            .output()
            .ok()?
            .stdout,
    )
    .ok()?;
    let rest = url.trim().split("github.com").nth(1)?;
    let rest = rest.trim_start_matches([':', '/']).trim_end_matches(".git");
    let mut it = rest.splitn(2, '/');
    Some((it.next()?.to_string(), it.next()?.trim_end_matches('/').to_string()))
}

async fn repo_root<A: API>(state: &AppState<A>) -> Option<std::path::PathBuf> {
    let cwd = state.api.environment().cwd;
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok().map(|s| std::path::PathBuf::from(s.trim()))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().user_agent("forge-web").build().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /api/board/platforms — which boards are available
// ---------------------------------------------------------------------------

pub(crate) async fn platforms<A: API>(State(state): State<AppState<A>>) -> Json<Value> {
    let gh = server(&state, "github").await.as_ref().and_then(github_pat).is_some();
    let jira = server(&state, "jira").await.as_ref().and_then(|s| stdio_env(s, "JIRA_URL")).is_some();
    let sentry = server(&state, "sentry").await.as_ref().and_then(|s| stdio_env(s, "SENTRY_ACCESS_TOKEN")).is_some();
    Json(json!({ "github": gh, "jira": jira, "sentry": sentry }))
}

// ---------------------------------------------------------------------------
// GitHub — open issues + PRs of the origin repo
// ---------------------------------------------------------------------------

pub(crate) async fn github_board<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let pat = server(&state, "github")
        .await
        .as_ref()
        .and_then(github_pat)
        .ok_or_else(|| AppError::bad_request("GitHub not connected"))?;
    let root = repo_root(&state).await.ok_or_else(|| AppError::bad_request("not a git repo"))?;
    let (owner, repo) = origin_slug(&root).ok_or_else(|| AppError::bad_request("no github origin remote"))?;

    let resp = client()
        .get(format!("https://api.github.com/repos/{owner}/{repo}/issues?state=open&per_page=50"))
        .header("Authorization", format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("github: {e}")))?;
    let arr: Value = resp.json().await.map_err(|e| AppError::bad_request(format!("github: {e}")))?;

    let mut issues = Vec::new();
    let mut prs = Vec::new();
    if let Some(list) = arr.as_array() {
        for it in list {
            let item = json!({
                "id": format!("#{}", it["number"].as_i64().unwrap_or(0)),
                "title": it["title"].as_str().unwrap_or(""),
                "url": it["html_url"].as_str().unwrap_or(""),
                "meta": it["user"]["login"].as_str().unwrap_or(""),
            });
            if it.get("pull_request").is_some() { prs.push(item); } else { issues.push(item); }
        }
    }
    Ok(Json(json!({ "columns": [
        { "name": "Issues", "items": issues },
        { "name": "Pull requests", "items": prs },
    ]})))
}

// ---------------------------------------------------------------------------
// Jira — my unresolved issues, grouped by status category
// ---------------------------------------------------------------------------

pub(crate) async fn jira_board<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let srv = server(&state, "jira").await.ok_or_else(|| AppError::bad_request("Jira not connected"))?;
    let url = stdio_env(&srv, "JIRA_URL").ok_or_else(|| AppError::bad_request("no JIRA_URL"))?;
    let email = stdio_env(&srv, "JIRA_USERNAME").unwrap_or_default();
    let token = stdio_env(&srv, "JIRA_API_TOKEN").unwrap_or_default();
    let base = url.trim_end_matches('/');
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));

    // Jira Cloud requires the (newer) /search/jql endpoint with a *bounded* JQL.
    let jql = "updated >= -30d ORDER BY updated DESC";
    let resp = client()
        .get(format!("{base}/rest/api/3/search/jql"))
        .query(&[("jql", jql), ("maxResults", "50"), ("fields", "summary,status")])
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("jira: {e}")))?;
    let v: Value = resp.json().await.map_err(|e| AppError::bad_request(format!("jira: {e}")))?;

    // Group into To Do / In Progress / Done by status category key.
    let mut todo = Vec::new();
    let mut prog = Vec::new();
    let mut done = Vec::new();
    if let Some(list) = v["issues"].as_array() {
        for it in list {
            let key = it["key"].as_str().unwrap_or("");
            let status = it["fields"]["status"]["name"].as_str().unwrap_or("");
            let cat = it["fields"]["status"]["statusCategory"]["key"].as_str().unwrap_or("new");
            let item = json!({
                "id": key,
                "title": it["fields"]["summary"].as_str().unwrap_or(""),
                "url": format!("{base}/browse/{key}"),
                "meta": status,
            });
            match cat {
                "done" => done.push(item),
                "indeterminate" => prog.push(item),
                _ => todo.push(item),
            }
        }
    }
    Ok(Json(json!({ "columns": [
        { "name": "To Do", "items": todo },
        { "name": "In Progress", "items": prog },
        { "name": "Done", "items": done },
    ]})))
}

// ---------------------------------------------------------------------------
// Sentry — unresolved issues, grouped by level
// ---------------------------------------------------------------------------

pub(crate) async fn sentry_board<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let srv = server(&state, "sentry").await.ok_or_else(|| AppError::bad_request("Sentry not connected"))?;
    let token = stdio_env(&srv, "SENTRY_ACCESS_TOKEN").ok_or_else(|| AppError::bad_request("no Sentry token"))?;
    let host = stdio_env(&srv, "SENTRY_HOST").unwrap_or_else(|| "sentry.io".to_string());
    let cl = client();
    let bearer = format!("Bearer {token}");

    // Discover the first org slug the token can see.
    let orgs: Value = cl
        .get(format!("https://{host}/api/0/organizations/"))
        .header("Authorization", &bearer)
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("sentry: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::bad_request(format!("sentry: {e}")))?;
    let org = orgs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|o| o["slug"].as_str())
        .ok_or_else(|| AppError::bad_request("no Sentry organization found"))?
        .to_string();

    let issues: Value = cl
        .get(format!("https://{host}/api/0/organizations/{org}/issues/"))
        .query(&[("query", "is:unresolved"), ("limit", "50"), ("statsPeriod", "14d")])
        .header("Authorization", &bearer)
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("sentry: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::bad_request(format!("sentry: {e}")))?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut other = Vec::new();
    if let Some(list) = issues.as_array() {
        for it in list {
            let level = it["level"].as_str().unwrap_or("error");
            let item = json!({
                "id": it["shortId"].as_str().unwrap_or(""),
                "title": it["title"].as_str().or_else(|| it["metadata"]["value"].as_str()).unwrap_or(""),
                "url": it["permalink"].as_str().unwrap_or(""),
                "meta": format!("{} events", it["count"].as_str().unwrap_or("0")),
            });
            match level {
                "error" | "fatal" => errors.push(item),
                "warning" => warnings.push(item),
                _ => other.push(item),
            }
        }
    }
    Ok(Json(json!({ "columns": [
        { "name": "Errors", "items": errors },
        { "name": "Warnings", "items": warnings },
        { "name": "Other", "items": other },
    ]})))
}
