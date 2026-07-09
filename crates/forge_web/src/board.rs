//! Read-only Kanban boards over the connected integrations.
//!
//! Rather than route through the agent, each board queries the platform's REST
//! API directly using the credentials the user already configured in their MCP
//! integrations (GitHub PAT, Jira URL/email/token, Sentry token). Everything is
//! read-only.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use base64::Engine;
use forge_api::API;
use forge_domain::McpServerConfig;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AppError, AppState};

// ---------------------------------------------------------------------------
// Web-only settings (not MCP): a tiny JSON file in $HOME. Currently just the
// Google Calendar private iCal URL.
// ---------------------------------------------------------------------------

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".forge-web.json"))
}

fn read_settings() -> Value {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_settings(v: &Value) {
    if let Some(p) = settings_path() {
        let _ = std::fs::write(p, serde_json::to_string_pretty(v).unwrap_or_default());
    }
}

#[derive(Deserialize)]
pub(crate) struct GcalBody {
    url: Option<String>,
}

/// GET /api/gcal — current Google Calendar iCal URL (if any).
pub(crate) async fn get_gcal<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    let url = read_settings().get("gcal_ics").and_then(|v| v.as_str()).map(str::to_string);
    Json(json!({ "url": url }))
}

/// PUT /api/gcal — set (or clear with an empty string) the iCal URL.
pub(crate) async fn set_gcal<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<GcalBody>,
) -> Json<Value> {
    let mut s = read_settings();
    match body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => { s["gcal_ics"] = json!(u); }
        None => { if let Value::Object(m) = &mut s { m.remove("gcal_ics"); } }
    }
    write_settings(&s);
    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// TODOs — a small personal list for the activity panel, persisted in the same
// settings file. Single-user local app, so plain read-modify-write is fine.
// ---------------------------------------------------------------------------

fn todos_from(s: &Value) -> Vec<Value> {
    s.get("todos").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// GET /api/todos — full list, newest last (insertion order).
pub(crate) async fn list_todos<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    Json(json!({ "todos": todos_from(&read_settings()) }))
}

#[derive(Deserialize)]
pub(crate) struct TodoAdd {
    text: String,
}

/// POST /api/todos — add one; returns the created todo.
pub(crate) async fn add_todo<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TodoAdd>,
) -> Result<Json<Value>, AppError> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(AppError::bad_request("todo text is empty"));
    }
    let mut s = read_settings();
    let mut todos = todos_from(&s);
    let todo = json!({
        "id": forge_domain::ConversationId::generate().into_string(),
        "text": text,
        "done": false,
        "created_at_ms": unix_ms(),
    });
    todos.push(todo.clone());
    s["todos"] = Value::Array(todos);
    write_settings(&s);
    Ok(Json(todo))
}

#[derive(Deserialize)]
pub(crate) struct TodoPatch {
    done: Option<bool>,
    text: Option<String>,
}

/// PUT /api/todos/{id} — set done and/or text.
pub(crate) async fn update_todo<A: API>(
    State(_): State<AppState<A>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<TodoPatch>,
) -> Result<Json<Value>, AppError> {
    let mut s = read_settings();
    let mut todos = todos_from(&s);
    let item = todos
        .iter_mut()
        .find(|t| t["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| AppError::not_found("no such todo"))?;
    if let Some(done) = body.done {
        item["done"] = json!(done);
    }
    if let Some(text) = body.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        item["text"] = json!(text);
    }
    let updated = item.clone();
    s["todos"] = Value::Array(todos);
    write_settings(&s);
    Ok(Json(updated))
}

/// Marks the start of an injected TODO-context block within a user message.
/// The block is only needed for the turn that produces it — once the model
/// has replied, [`crate::live`] truncates any stored user message at this
/// marker so it never lingers in history/export views.
pub(crate) const TODOS_CONTEXT_MARKER: &str = "\n\n<forge_web_todos>";

/// Renders the panel's TODO list as a context block appended to chat turns
/// that mention todos. The agent's own `todo_read` tool is session-scoped and
/// can't see this list, so we hand it over explicitly — including where it
/// lives, so the agent can check items off with its file tools.
pub(crate) fn todos_context() -> String {
    let todos = todos_from(&read_settings());
    if todos.is_empty() {
        return String::new();
    }
    let path = settings_path().map(|p| p.display().to_string()).unwrap_or_default();
    let mut s = format!(
        "{TODOS_CONTEXT_MARKER}\nThe user's TODO list from the Forge web panel. It is stored as \
         JSON under the \"todos\" key in {path}; each item is an object with \"id\", \"text\" and a \
         boolean \"done\". Current items:\n"
    );
    for t in &todos {
        let done = t["done"].as_bool().unwrap_or(false);
        s.push_str(&format!(
            "- id={} done={} — {}\n",
            t["id"].as_str().unwrap_or(""),
            done,
            t["text"].as_str().unwrap_or("")
        ));
    }
    s.push_str(
        "\nIMPORTANT: If the user asks you to execute / do / run / complete one of these items \
         (e.g. \"执行todo\", \"完成这个待办\"), then after you finish the work you MUST mark it \
         done: edit the JSON file above and set \"done\": true on the object with the matching \
         \"id\", leaving every other field and item unchanged. This is what makes the panel show \
         the item as completed — do it as your final step, and confirm it in your reply.\n\
         </forge_web_todos>",
    );
    s
}

/// DELETE /api/todos/{id}
pub(crate) async fn delete_todo<A: API>(
    State(_): State<AppState<A>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<Value> {
    let mut s = read_settings();
    let mut todos = todos_from(&s);
    todos.retain(|t| t["id"].as_str() != Some(id.as_str()));
    s["todos"] = Value::Array(todos);
    write_settings(&s);
    Json(json!({ "ok": true }))
}

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

/// Parses `(owner, repo)` from a GitHub remote URL (https or `git@` ssh form).
fn parse_github_slug(url: &str) -> Option<(String, String)> {
    let rest = url.trim().split("github.com").nth(1)?;
    let rest = rest.trim_start_matches([':', '/']).trim_end_matches(".git");
    let mut it = rest.splitn(2, '/');
    Some((it.next()?.to_string(), it.next()?.trim_end_matches('/').to_string()))
}

/// All GitHub remotes as `(remote_name, owner, repo)`, de-duplicated across the
/// fetch/push pair that `git remote -v` prints for each remote.
fn github_remotes(git_root: &Path) -> Vec<(String, String, String)> {
    let output = match Command::new("git")
        .args(["remote", "-v"])
        .current_dir(git_root)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            continue; // second (push) line for a remote already recorded
        }
        if let Some((owner, repo)) = parse_github_slug(url) {
            out.push((name.to_string(), owner, repo));
        }
    }
    out
}

/// The login of the account a GitHub token belongs to (`GET /user`).
async fn github_login(cl: &reqwest::Client, pat: &str) -> Option<String> {
    let resp = cl
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    resp.json::<Value>()
        .await
        .ok()?
        .get("login")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Resolves the `(owner, repo)` the GitHub boards should track.
///
/// A forked checkout usually leaves `origin` pointing at the *upstream* repo,
/// with the user's own fork under a differently named remote — so blindly using
/// `origin` shows someone else's issues/CI. Prefer the remote whose owner
/// matches the token's account (your fork); fall back to `origin`, then the
/// first GitHub remote.
async fn resolve_repo(cl: &reqwest::Client, pat: &str, git_root: &Path) -> Option<(String, String)> {
    let remotes = github_remotes(git_root);
    if remotes.is_empty() {
        return None;
    }
    if let Some(login) = github_login(cl, pat).await
        && let Some((_, owner, repo)) = remotes
            .iter()
            .find(|(_, owner, _)| owner.eq_ignore_ascii_case(&login))
    {
        return Some((owner.clone(), repo.clone()));
    }
    remotes
        .iter()
        .find(|(name, _, _)| name == "origin")
        .or_else(|| remotes.first())
        .map(|(_, owner, repo)| (owner.clone(), repo.clone()))
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
    let gcal = read_settings().get("gcal_ics").and_then(|v| v.as_str()).is_some();
    // GitHub Actions reuses the GitHub connection.
    Json(json!({ "github": gh, "gha": gh, "jira": jira, "sentry": sentry, "gcal": gcal }))
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
    let cl = client();
    let (owner, repo) = resolve_repo(&cl, &pat, &root)
        .await
        .ok_or_else(|| AppError::bad_request("no github remote"))?;

    // Exact counts via the search API (no page cap), fetched concurrently.
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
// GitHub Actions — CI health on the default branch (reuses the GitHub PAT)
// ---------------------------------------------------------------------------

pub(crate) async fn gha_board<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let pat = server(&state, "github")
        .await
        .as_ref()
        .and_then(github_pat)
        .ok_or_else(|| AppError::bad_request("GitHub not connected"))?;
    let root = repo_root(&state).await.ok_or_else(|| AppError::bad_request("not a git repo"))?;
    let cl = client();
    let (owner, repo) = resolve_repo(&cl, &pat, &root)
        .await
        .ok_or_else(|| AppError::bad_request("no github remote"))?;
    let auth = format!("Bearer {pat}");

    // Default branch (fall back to "main").
    let branch = cl
        .get(format!("https://api.github.com/repos/{owner}/{repo}"))
        .header("Authorization", &auth)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok();
    let branch = match branch {
        Some(r) => r.json::<Value>().await.ok().and_then(|v| v["default_branch"].as_str().map(str::to_string)).unwrap_or_else(|| "main".into()),
        None => "main".into(),
    };

    let runs: Value = cl
        .get(format!("https://api.github.com/repos/{owner}/{repo}/actions/runs"))
        .query(&[("branch", branch.as_str()), ("per_page", "50")])
        .header("Authorization", &auth)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("actions: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::bad_request(format!("actions: {e}")))?;

    // Keep only the most recent run per workflow (the list is newest-first),
    // so the counts reflect each workflow's current state.
    let mut seen = std::collections::HashSet::new();
    let (mut passing, mut failing, mut running) = (0, 0, 0);
    let mut latest: Option<(String, String)> = None; // (conclusion/status, workflow name)
    if let Some(list) = runs["workflow_runs"].as_array() {
        for r in list {
            let key = r["workflow_id"].as_i64().map(|i| i.to_string()).unwrap_or_else(|| r["name"].as_str().unwrap_or("").to_string());
            if !seen.insert(key) {
                continue;
            }
            let status = r["status"].as_str().unwrap_or("");
            let concl = r["conclusion"].as_str().unwrap_or("");
            if latest.is_none() {
                let s = if status != "completed" { status } else { concl };
                latest = Some((s.to_string(), r["name"].as_str().unwrap_or("").to_string()));
            }
            if status != "completed" {
                running += 1;
            } else {
                match concl {
                    "success" => passing += 1,
                    "failure" | "startup_failure" | "timed_out" => failing += 1,
                    _ => {} // cancelled / skipped / neutral — ignore
                }
            }
        }
    }

    let subtitle = if failing > 0 {
        format!("{branch} · {failing} failing")
    } else if running > 0 {
        format!("{branch} · building")
    } else if passing > 0 {
        format!("{branch} · all green")
    } else {
        format!("{branch} · no runs")
    };

    Ok(Json(json!({
        "url": format!("https://github.com/{owner}/{repo}/actions?query=branch%3A{branch}"),
        "subtitle": subtitle,
        "stats": [
            { "label": "Passing", "value": passing },
            { "label": "Failing", "value": failing },
            { "label": "Running", "value": running },
        ]
    })))
}

// ---------------------------------------------------------------------------
// GET /api/pipelines — CI runs currently queued / in progress, for the
// activity panel. Empty list (not an error) when GitHub isn't connected or the
// cwd isn't a GitHub repo, so the panel can just show "none".
// ---------------------------------------------------------------------------

pub(crate) async fn running_pipelines<A: API>(State(state): State<AppState<A>>) -> Json<Value> {
    let empty = || Json(json!({ "pipelines": [] }));
    let Some(pat) = server(&state, "github").await.as_ref().and_then(github_pat) else {
        return empty();
    };
    let Some(root) = repo_root(&state).await else { return empty() };
    let cl = client();
    let Some((owner, repo)) = resolve_repo(&cl, &pat, &root).await else { return empty() };

    let runs: Value = match cl
        .get(format!("https://api.github.com/repos/{owner}/{repo}/actions/runs"))
        .query(&[("per_page", "50")])
        .header("Authorization", format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_else(|_| json!({})),
        Err(_) => return empty(),
    };

    // In-progress runs are the newest, so one recent page is enough.
    let mut out = Vec::new();
    for r in runs["workflow_runs"].as_array().unwrap_or(&Vec::new()) {
        if r["status"].as_str().unwrap_or("completed") == "completed" {
            continue;
        }
        out.push(json!({
            "name": r["name"].as_str().unwrap_or("workflow"),
            "branch": r["head_branch"].as_str().unwrap_or(""),
            "status": r["status"].as_str().unwrap_or(""),
            "url": r["html_url"].as_str().unwrap_or(""),
        }));
        if out.len() >= 10 {
            break;
        }
    }
    Json(json!({ "pipelines": out }))
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

// ---------------------------------------------------------------------------
// Google Calendar — upcoming events from the private iCal (.ics) URL
// ---------------------------------------------------------------------------

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// One parsed calendar event: its start (epoch-day + minute-of-day) and title.
struct Event {
    day: i64,
    minute: i64,
    all_day: bool,
    summary: String,
    freq: Option<String>,
    interval: i64,
}

/// Unfolds RFC 5545 line folding (continuation lines start with space/tab).
fn unfold(ics: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in ics.split('\n') {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// The machine's current UTC offset in seconds (via `date +%z`, e.g. +0800).
/// Used to render calendar times in the viewer's local zone.
fn local_offset_seconds() -> i64 {
    let s = Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let s = s.trim();
    if s.len() >= 5 {
        let sign = if s.starts_with('-') { -1 } else { 1 };
        let hh: i64 = s[1..3].parse().unwrap_or(0);
        let mm: i64 = s[3..5].parse().unwrap_or(0);
        return sign * (hh * 3600 + mm * 60);
    }
    0
}

/// Parses a DTSTART value into (local epoch-day, local minute-of-day, all_day).
///
/// Times marked UTC (`…Z`) are shifted by `offset` into local time; times with
/// a TZID or none are treated as already-local (the common "my calendar in my
/// own zone" case). All-day dates are never shifted.
fn parse_dtstart(prop: &str, value: &str, offset: i64) -> Option<(i64, i64, bool)> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    let y: i64 = digits[0..4].parse().ok()?;
    let m: i64 = digits[4..6].parse().ok()?;
    let d: i64 = digits[6..8].parse().ok()?;
    let base_day = days_from_civil(y, m, d);
    let all_day = prop.contains("VALUE=DATE") || digits.len() < 12;
    if all_day {
        return Some((base_day, 0, true));
    }
    let hh: i64 = digits[8..10].parse().unwrap_or(0);
    let mm: i64 = digits[10..12].parse().unwrap_or(0);
    let ss: i64 = if digits.len() >= 14 { digits[12..14].parse().unwrap_or(0) } else { 0 };
    let mut secs = base_day * 86400 + hh * 3600 + mm * 60 + ss;
    if value.trim_end().ends_with('Z') {
        secs += offset; // UTC → local
    }
    Some((secs.div_euclid(86400), secs.rem_euclid(86400) / 60, false))
}

fn parse_ics(ics: &str, offset: i64) -> Vec<Event> {
    let mut events = Vec::new();
    let mut cur: Option<Event> = None;
    for line in unfold(ics) {
        if line == "BEGIN:VEVENT" {
            cur = Some(Event { day: 0, minute: 0, all_day: false, summary: String::new(), freq: None, interval: 1 });
        } else if line == "END:VEVENT" {
            if let Some(e) = cur.take() {
                if e.day != 0 {
                    events.push(e);
                }
            }
        } else if let Some(e) = cur.as_mut() {
            let (name, value) = match line.split_once(':') {
                Some(x) => x,
                None => continue,
            };
            let key = name.split(';').next().unwrap_or(name);
            match key {
                "SUMMARY" => e.summary = value.replace("\\,", ",").replace("\\n", " ").trim().to_string(),
                "DTSTART" => {
                    if let Some((day, minute, all_day)) = parse_dtstart(name, value, offset) {
                        e.day = day;
                        e.minute = minute;
                        e.all_day = all_day;
                    }
                }
                "RRULE" => {
                    for part in value.split(';') {
                        if let Some(f) = part.strip_prefix("FREQ=") {
                            e.freq = Some(f.to_string());
                        } else if let Some(i) = part.strip_prefix("INTERVAL=") {
                            e.interval = i.parse().unwrap_or(1).max(1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    events
}

/// Does `event` have an occurrence exactly on `target` (an epoch-day), within a
/// short look-ahead window? Expands simple DAILY / WEEKLY recurrences.
fn occurs_on(e: &Event, target: i64) -> bool {
    if target < e.day {
        return false;
    }
    match e.freq.as_deref() {
        None => e.day == target,
        Some("DAILY") => (target - e.day) % e.interval == 0,
        Some("WEEKLY") => (target - e.day) % (7 * e.interval) == 0,
        // Monthly/Yearly: only count the original date if it lands in-window.
        _ => e.day == target,
    }
}

pub(crate) async fn gcal_board<A: API>(
    State(_): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let url = read_settings()
        .get("gcal_ics")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| AppError::bad_request("Google Calendar not connected"))?;

    let text = client()
        .get(&url)
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("calendar: {e}")))?
        .text()
        .await
        .map_err(|e| AppError::bad_request(format!("calendar: {e}")))?;
    if !text.contains("BEGIN:VCALENDAR") {
        return Err(AppError::bad_request("that URL didn't return an iCal feed"));
    }
    let offset = local_offset_seconds();
    let events = parse_ics(&text, offset);

    // Work entirely in local time so "today"/times line up with the calendar.
    let utc = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let local = utc + offset;
    let today = local.div_euclid(86400);
    let now_minute = local.rem_euclid(86400) / 60;

    // Current calendar week, Monday–Sunday (epoch day 0 = Thursday).
    let dow_mon = (today + 3).rem_euclid(7); // Mon=0 … Sun=6
    let week_start = today - dow_mon;
    let week_end = week_start + 6;

    // Scan from the start of this week through a 14-day look-ahead.
    let scan_start = today.min(week_start);
    let scan_end = today + 13;
    let mut today_count = 0;
    let mut this_week = 0;
    let mut next7 = 0;
    let mut next: Option<(i64, i64, String)> = None; // (day, minute, summary)
    for e in &events {
        let mut day = scan_start;
        while day <= scan_end {
            if occurs_on(e, day) {
                if day == today {
                    today_count += 1;
                }
                if day >= week_start && day <= week_end {
                    this_week += 1;
                }
                if day >= today && day <= today + 6 {
                    next7 += 1;
                }
                let is_future = day > today || (day == today && (e.all_day || e.minute >= now_minute));
                if is_future {
                    let cand = (day, e.minute, e.summary.clone());
                    if next.as_ref().map(|n| (cand.0, cand.1) < (n.0, n.1)).unwrap_or(true) {
                        next = Some(cand);
                    }
                }
            }
            day += 1;
        }
    }

    let subtitle = match &next {
        Some((day, minute, summary)) => {
            let when = if *day == today {
                if *minute > 0 { format!("today {:02}:{:02}", minute / 60, minute % 60) } else { "today".to_string() }
            } else if *day == today + 1 {
                if *minute > 0 { format!("tomorrow {:02}:{:02}", minute / 60, minute % 60) } else { "tomorrow".to_string() }
            } else {
                format!("in {} days", day - today)
            };
            let title = if summary.is_empty() { "(no title)" } else { summary.as_str() };
            let title: String = title.chars().take(40).collect();
            format!("Next: {title} · {when}")
        }
        None => "No upcoming events".to_string(),
    };

    Ok(Json(json!({
        "url": "https://calendar.google.com/calendar/r",
        "subtitle": subtitle,
        "stats": [
            { "label": "Today", "value": today_count },
            { "label": "This week", "value": this_week },
            { "label": "Next 7 days", "value": next7 },
        ]
    })))
}
