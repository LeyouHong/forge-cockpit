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
    let gcal = read_settings().get("gcal_ics").and_then(|v| v.as_str()).is_some();
    Json(json!({ "github": gh, "jira": jira, "sentry": sentry, "gcal": gcal }))
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

/// Parses a DTSTART value into (epoch-day, minute-of-day, all_day).
fn parse_dtstart(prop: &str, value: &str) -> Option<(i64, i64, bool)> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    let y: i64 = digits[0..4].parse().ok()?;
    let m: i64 = digits[4..6].parse().ok()?;
    let d: i64 = digits[6..8].parse().ok()?;
    let day = days_from_civil(y, m, d);
    let all_day = prop.contains("VALUE=DATE") || digits.len() < 12;
    let minute = if all_day {
        0
    } else {
        let hh: i64 = digits[8..10].parse().unwrap_or(0);
        let mm: i64 = digits[10..12].parse().unwrap_or(0);
        hh * 60 + mm
    };
    Some((day, minute, all_day))
}

fn parse_ics(ics: &str) -> Vec<Event> {
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
                    if let Some((day, minute, all_day)) = parse_dtstart(name, value) {
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
    let events = parse_ics(&text);

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let today = now / 86400;
    let now_minute = (now % 86400) / 60;

    // Count today / next-7-days occurrences over a 14-day look-ahead.
    let mut today_count = 0;
    let mut week_count = 0;
    let mut next: Option<(i64, i64, String)> = None; // (day, minute, summary)
    for e in &events {
        for k in 0..14 {
            let target = today + k;
            if occurs_on(e, target) {
                if target == today {
                    today_count += 1;
                }
                if k < 7 {
                    week_count += 1;
                }
                // Track the soonest upcoming occurrence (future, or later today).
                let is_future = target > today || (target == today && (e.all_day || e.minute >= now_minute));
                if is_future {
                    let cand = (target, e.minute, e.summary.clone());
                    if next.as_ref().map(|n| (cand.0, cand.1) < (n.0, n.1)).unwrap_or(true) {
                        next = Some(cand);
                    }
                }
            }
        }
    }

    let subtitle = match &next {
        Some((day, minute, summary)) => {
            let when = if *day == today {
                if *minute > 0 { format!("today {:02}:{:02}", minute / 60, minute % 60) } else { "today".to_string() }
            } else if *day == today + 1 {
                "tomorrow".to_string()
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
            { "label": "Next 7 days", "value": week_count },
        ]
    })))
}
