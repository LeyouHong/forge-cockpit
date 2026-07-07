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

/// Exact hit count for a GitHub search query (0 on any error).
async fn gh_count(cl: &reqwest::Client, pat: &str, q: &str) -> i64 {
    let resp = match cl
        .get("https://api.github.com/search/issues")
        .query(&[("q", q), ("per_page", "1")])
        .header("Authorization", format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return 0,
    };
    resp.json::<Value>().await.ok().and_then(|v| v["total_count"].as_i64()).unwrap_or(0)
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

    // Exact counts via the search API (no page cap), fetched concurrently.
    let cl = client();
    let scope = format!("repo:{owner}/{repo}");
    let q_issues = format!("{scope} is:open is:issue");
    let q_prs = format!("{scope} is:open is:pr");
    let q_assigned = format!("{scope} is:open is:issue assignee:@me");
    let q_reviews = format!("{scope} is:open is:pr review-requested:@me");
    let (issues, prs, assigned, reviews) = tokio::join!(
        gh_count(&cl, &pat, &q_issues),
        gh_count(&cl, &pat, &q_prs),
        gh_count(&cl, &pat, &q_assigned),
        gh_count(&cl, &pat, &q_reviews),
    );
    let base = format!("https://github.com/{owner}/{repo}");
    Ok(Json(json!({
        "url": base,
        "subtitle": format!("{owner}/{repo}"),
        "stats": [
            { "label": "Open issues", "value": issues, "url": format!("{base}/issues") },
            { "label": "Open PRs", "value": prs, "url": format!("{base}/pulls") },
            { "label": "Assigned to me", "value": assigned, "url": format!("{base}/issues?q=is%3Aopen+assignee%3A%40me") },
            { "label": "Review requests", "value": reviews, "url": format!("{base}/pulls?q=is%3Aopen+review-requested%3A%40me") },
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

    let cl = client();
    // Jira Cloud requires the (newer) /search/jql endpoint with a *bounded* JQL.
    let jql = "updated >= -30d ORDER BY updated DESC";
    let resp = cl
        .get(format!("{base}/rest/api/3/search/jql"))
        .query(&[("jql", jql), ("maxResults", "50"), ("fields", "summary,status")])
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("jira: {e}")))?;
    let v: Value = resp.json().await.map_err(|e| AppError::bad_request(format!("jira: {e}")))?;

    // Count the last-30d window by status category.
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

    // Issues assigned to me (open) — an approximate count avoids paging.
    let mine = cl
        .post(format!("{base}/rest/api/3/search/approximate-count"))
        .header("Authorization", format!("Basic {auth}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({ "jql": "assignee = currentUser() AND statusCategory != Done" }))
        .send()
        .await
        .ok();
    let mine = match mine {
        Some(r) => r.json::<Value>().await.ok().and_then(|v| v["count"].as_i64()).unwrap_or(0),
        None => 0,
    };

    Ok(Json(json!({
        "url": format!("{base}/issues"),
        "subtitle": "last 30 days",
        "stats": [
            { "label": "To Do", "value": todo },
            { "label": "In Progress", "value": prog },
            { "label": "Done", "value": done },
            { "label": "Assigned to me", "value": mine },
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

    // Newly-seen issues in the last 24h.
    let new24 = cl
        .get(format!("https://{host}/api/0/organizations/{org}/issues/"))
        .query(&[("query", "is:unresolved firstSeen:-24h"), ("limit", "100")])
        .header("Authorization", &bearer)
        .send()
        .await
        .ok();
    let new24 = match new24 {
        Some(r) => r.json::<Value>().await.ok().and_then(|v| v.as_array().map(|a| a.len())).unwrap_or(0),
        None => 0,
    };

    Ok(Json(json!({
        "url": format!("https://{host}/organizations/{org}/issues/"),
        "subtitle": format!("org: {org} · unresolved · 14d"),
        "stats": [
            { "label": "Errors", "value": errors, "suffix": suffix },
            { "label": "Warnings", "value": warnings },
            { "label": "Other", "value": other },
            { "label": "New 24h", "value": new24 },
        ]
    })))
}
