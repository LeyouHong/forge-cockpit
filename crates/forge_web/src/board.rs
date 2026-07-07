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

    let (mut issues, mut prs) = (0, 0);
    if let Some(list) = arr.as_array() {
        for it in list {
            if it.get("pull_request").is_some() { prs += 1 } else { issues += 1 }
        }
    }
    // The `issues` endpoint returns issues + PRs; note the 50-item page cap.
    let capped = arr.as_array().map(|a| a.len() >= 50).unwrap_or(false);
    Ok(Json(json!({
        "url": format!("https://github.com/{owner}/{repo}"),
        "subtitle": format!("{owner}/{repo}"),
        "stats": [
            { "label": "Open issues", "value": issues, "suffix": if capped { "+" } else { "" }, "url": format!("https://github.com/{owner}/{repo}/issues") },
            { "label": "Open PRs", "value": prs, "url": format!("https://github.com/{owner}/{repo}/pulls") },
        ]
    })))
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

    // Count by status category.
    let (mut todo, mut prog, mut done) = (0, 0, 0);
    if let Some(list) = v["issues"].as_array() {
        for it in list {
            match it["fields"]["status"]["statusCategory"]["key"].as_str().unwrap_or("new") {
                "done" => done += 1,
                "indeterminate" => prog += 1,
                _ => todo += 1,
            }
        }
    }
    Ok(Json(json!({
        "url": format!("{base}/issues"),
        "subtitle": "last 30 days",
        "stats": [
            { "label": "To Do", "value": todo },
            { "label": "In Progress", "value": prog },
            { "label": "Done", "value": done },
        ]
    })))
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

    let (mut errors, mut warnings, mut other) = (0, 0, 0);
    if let Some(list) = issues.as_array() {
        for it in list {
            match it["level"].as_str().unwrap_or("error") {
                "error" | "fatal" => errors += 1,
                "warning" => warnings += 1,
                _ => other += 1,
            }
        }
    }
    let capped = issues.as_array().map(|a| a.len() >= 50).unwrap_or(false);
    let suffix = if capped { "+" } else { "" };
    Ok(Json(json!({
        "url": format!("https://{host}/organizations/{org}/issues/"),
        "subtitle": format!("org: {org} · unresolved · 14d"),
        "stats": [
            { "label": "Errors", "value": errors, "suffix": suffix },
            { "label": "Warnings", "value": warnings },
            { "label": "Other", "value": other },
        ]
    })))
}
