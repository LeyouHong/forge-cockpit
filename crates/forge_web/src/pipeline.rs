//! The Pipeline page — manage projects, edit workflow YAML, run them, and view
//! runs. A thin HTTP surface over the `forge_workspace` pipeline engine:
//!
//!   - projects: named directories, persisted in the web settings file.
//!   - files: workflow YAMLs under `<project>/.forge/pipelines/`.
//!   - validate: `forge_workspace::pipeline::parse_workflow`.
//!   - run: spawn the `forge-pipeline` binary against the project.
//!   - runs: read `<project>/.forge-workspace/pipelines/*.yml`.
//!
//! Like the rest of `/api/*`, these run commands/agents as the user; the page is
//! gated behind the per-run bearer token.

use std::path::PathBuf;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use axum::Json;
use axum::extract::{Query, State};
use forge_api::API;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::board::{read_settings, write_settings};
use crate::{AppError, AppState};

// ─── Projects (persisted in .forge-web.json under "pipeline_projects") ───────

fn projects_from(s: &Value) -> Vec<Value> {
    s.get("pipeline_projects").and_then(Value::as_array).cloned().unwrap_or_default()
}

pub(crate) fn project_path_pub(name: &str) -> Option<PathBuf> {
    project_path(name)
}
fn project_path(name: &str) -> Option<PathBuf> {
    projects_from(&read_settings())
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|p| p.get("path").and_then(Value::as_str))
        .map(PathBuf::from)
}

/// GET /api/pipeline/projects
pub(crate) async fn list_projects<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    Json(json!({ "projects": projects_from(&read_settings()) }))
}

#[derive(Deserialize)]
pub(crate) struct BrowseQuery {
    path: Option<String>,
}

/// GET /api/pipeline/browse?path=DIR — list a folder's subdirectories, for the
/// server-side folder picker (the server is local, so this is the user's FS).
/// Defaults to $HOME. Hidden dot-folders are skipped; git repos are flagged.
pub(crate) async fn browse<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<BrowseQuery>,
) -> Json<Value> {
    let start = q
        .path
        .map(|p| PathBuf::from(shellexpand(&p)))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(home_dir);
    let path = start.canonicalize().unwrap_or_else(|_| home_dir());
    let mut dirs: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&path) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
                dirs.push(json!({ "name": name, "git": p.join(".git").exists() }));
            }
        }
    }
    dirs.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    Json(json!({ "path": path.to_string_lossy(), "parent": parent, "dirs": dirs }))
}

fn home_dir() -> PathBuf {
    forge_workspace::pipeline::home_dir()
}

#[derive(Deserialize)]
pub(crate) struct ProjectAdd {
    path: String,
    name: Option<String>,
}

/// POST /api/pipeline/projects — add a project directory.
pub(crate) async fn add_project<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<ProjectAdd>,
) -> Result<Json<Value>, AppError> {
    let path = PathBuf::from(shellexpand(&body.path));
    let path = path.canonicalize().map_err(|_| AppError::bad_request(format!("no such directory: {}", body.path)))?;
    if !path.is_dir() {
        return Err(AppError::bad_request("path is not a directory"));
    }
    let name = body
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".into()));

    let mut s = read_settings();
    let mut list = projects_from(&s);
    if list.iter().any(|p| p.get("name").and_then(Value::as_str) == Some(name.as_str())) {
        return Err(AppError::bad_request(format!("a project named `{name}` already exists")));
    }
    let project = json!({ "name": name, "path": path.to_string_lossy() });
    list.push(project.clone());
    s["pipeline_projects"] = Value::Array(list);
    write_settings(&s);
    Ok(Json(project))
}

#[derive(Deserialize)]
pub(crate) struct ProjectRef {
    name: String,
}

/// DELETE /api/pipeline/projects — forget a project (does not touch files).
pub(crate) async fn remove_project<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<ProjectRef>,
) -> Json<Value> {
    let mut s = read_settings();
    let list: Vec<Value> = projects_from(&s)
        .into_iter()
        .filter(|p| p.get("name").and_then(Value::as_str) != Some(body.name.as_str()))
        .collect();
    s["pipeline_projects"] = Value::Array(list);
    write_settings(&s);
    Json(json!({ "ok": true }))
}

// ─── Pipeline files (global: ~/.forge-web/pipelines/*.yaml) ─────────────────
// Pipelines are reusable recipes, independent of any project. The target working
// directory and inputs are supplied at RUN time (see run_pipeline), and runs
// persist globally too. `project` is only for the Team page.

/// Where global pipeline files live (shared with the CLI + agent tools).
fn global_pipelines_dir() -> PathBuf {
    forge_workspace::pipeline::global_pipelines_dir()
}
/// The workspace all runs persist to (global, not per project).
fn runs_ws() -> PathBuf {
    forge_workspace::pipeline::global_runs_workspace()
}

/// Resolve `~/.forge-web/pipelines/<name>`, rejecting path traversal.
fn pipeline_file(file: &str) -> Result<PathBuf, AppError> {
    if !forge_workspace::pipeline::validate_pipeline_name(file) {
        return Err(AppError::bad_request("invalid pipeline name"));
    }
    if !(file.ends_with(".yaml") || file.ends_with(".yml")) {
        return Err(AppError::bad_request("pipeline name must end in .yaml/.yml"));
    }
    Ok(global_pipelines_dir().join(file))
}

/// Joins a browser-supplied relative path onto a project directory, refusing
/// anything that could escape it.
///
/// A `contains("..")` check alone is not enough: `Path::join` *discards the base
/// entirely* when the argument is absolute, so `path=/etc/passwd` sails through
/// a `..`-only guard and reads straight off the filesystem root. Reject absolute
/// paths (and Windows drive prefixes) as well as `..` and empty.
fn project_rel(project: &std::path::Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = rel.trim();
    if rel.is_empty() || rel.contains("..") {
        return Err(AppError::bad_request("invalid path"));
    }
    let p = std::path::Path::new(rel);
    let escapes = p.is_absolute()
        || p.components().any(|c| {
            matches!(c, std::path::Component::RootDir | std::path::Component::Prefix(_))
        });
    if escapes {
        return Err(AppError::bad_request("invalid path"));
    }
    Ok(project.join(rel))
}

#[derive(Deserialize)]
pub(crate) struct ProjectQuery {
    project: String,
}

/// GET /api/pipeline/files — list all (global) workflow files.
pub(crate) async fn list_files<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    let dir = global_pipelines_dir();
    let mut files: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Some(n) = e.file_name().to_str() {
                if n.ends_with(".yaml") || n.ends_with(".yml") {
                    files.push(n.to_string());
                }
            }
        }
    }
    files.sort();
    Json(json!({ "dir": dir.to_string_lossy(), "files": files }))
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    name: String,
}

/// GET /api/pipeline/file?name=FILE — read a workflow + validation.
pub(crate) async fn read_file<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&q.name)?;
    let content = std::fs::read_to_string(&path).map_err(|_| AppError::not_found("no such pipeline"))?;
    let (valid, error) = validate(&content);
    Ok(Json(json!({ "name": q.name, "content": content, "valid": valid, "error": error })))
}

#[derive(Deserialize)]
pub(crate) struct FileSave {
    name: String,
    content: String,
}

/// PUT /api/pipeline/file — save (creating if new). Always saves; reports the
/// validation result so the editor can show it without losing work.
pub(crate) async fn save_file<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<FileSave>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&body.name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &body.content)?;
    let (valid, error) = validate(&body.content);
    Ok(Json(json!({ "ok": true, "valid": valid, "error": error })))
}

/// POST /api/pipeline/delete — delete a workflow file.
pub(crate) async fn delete_file<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<FileQuery>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&q.name)?;
    std::fs::remove_file(&path).map_err(|_| AppError::not_found("no such pipeline"))?;
    Ok(Json(json!({ "ok": true })))
}

/// The layout sidecar next to a pipeline: `<name>.layout.json` holding node xy.
fn layout_path(file: &str) -> Result<PathBuf, AppError> {
    Ok(pipeline_file(file)?.with_extension("layout.json"))
}

/// GET /api/pipeline/graph?project=&name= — the workflow as JSON (for the visual
/// editor), plus validation and the saved node layout. YAML→JSON is done here so
/// the browser never needs a YAML parser.
pub(crate) async fn read_graph<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&q.name)?;
    let content = std::fs::read_to_string(&path).map_err(|_| AppError::not_found("no such pipeline"))?;
    let workflow: Value = serde_yml::from_str(&content).unwrap_or_else(|_| json!({}));
    let (valid, error) = validate(&content);
    let layout: Value = layout_path(&q.name)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({ "workflow": workflow, "valid": valid, "error": error, "layout": layout })))
}

#[derive(Deserialize)]
pub(crate) struct GraphSave {
    name: String,
    workflow: Value,
    layout: Option<Value>,
}

/// PUT /api/pipeline/graph — the visual editor's workflow JSON, serialized to
/// YAML (JSON→YAML here) and written; the node layout goes to the sidecar.
pub(crate) async fn save_graph<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<GraphSave>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&body.name)?;
    let yaml = serde_yml::to_string(&body.workflow)
        .map_err(|e| AppError::bad_request(format!("serialize workflow to yaml: {e}")))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &yaml)?;
    if let (Some(layout), Ok(lp)) = (&body.layout, layout_path(&body.name)) {
        let _ = std::fs::write(lp, serde_json::to_string_pretty(layout).unwrap_or_default());
    }
    let (valid, error) = validate(&yaml);
    Ok(Json(json!({ "ok": true, "valid": valid, "error": error, "yaml": yaml })))
}

#[derive(Deserialize)]
pub(crate) struct ValidateBody {
    content: String,
}

/// POST /api/pipeline/validate — parse-check a workflow without saving.
pub(crate) async fn validate_content<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<ValidateBody>,
) -> Json<Value> {
    let (valid, error) = validate(&body.content);
    Json(json!({ "valid": valid, "error": error }))
}

#[derive(Deserialize)]
pub(crate) struct GraphBody {
    workflow: Value,
}

/// POST /api/pipeline/validate-graph — validate the visual editor's JSON
/// workflow (serialize to YAML, parse-check) without writing anything.
pub(crate) async fn validate_graph<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<GraphBody>,
) -> Json<Value> {
    match serde_yml::to_string(&body.workflow) {
        Ok(yaml) => {
            let (valid, error) = validate(&yaml);
            Json(json!({ "valid": valid, "error": error, "yaml": yaml }))
        }
        Err(e) => Json(json!({ "valid": false, "error": format!("serialize: {e}") })),
    }
}

fn validate(content: &str) -> (bool, Option<String>) {
    match forge_workspace::pipeline::parse_workflow(content) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(format!("{e:#}"))),
    }
}

// ─── Run + runs ─────────────────────────────────────────────────────────────

fn forge_pipeline_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("forge-pipeline")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("forge-pipeline"))
}

#[derive(Deserialize)]
pub(crate) struct RunReq {
    name: String,
    /// Target working directory nodes default to (their `project:` field wins).
    #[serde(default)]
    dir: Option<String>,
    /// Values for the workflow's declared `input:` fields.
    #[serde(default)]
    inputs: std::collections::BTreeMap<String, String>,
    /// When true, run with the user's FULL MCP config (Gmail/Slack/…) instead
    /// of the isolated workspace-only MCP — so agent nodes can use connected
    /// tools (e.g. send an email). Default false = isolated (fast, shareable).
    #[serde(default)]
    use_mcp: bool,
}

/// POST /api/pipeline/run — run a (global) workflow against a target directory
/// with the given inputs, in the background. Runs persist to the global runs ws.
pub(crate) async fn run_pipeline<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<RunReq>,
) -> Result<Json<Value>, AppError> {
    let file = pipeline_file(&body.name)?;
    if !file.exists() {
        return Err(AppError::not_found("no such pipeline"));
    }
    let project = body
        .dir
        .as_deref()
        .map(|d| PathBuf::from(shellexpand(d)))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(home_dir);
    let project = project.canonicalize().map_err(|_| AppError::bad_request("target directory does not exist"))?;
    let ws = runs_ws();
    std::fs::create_dir_all(ws.join("pipelines"))?;
    let log = ws.join("pipelines").join(".last-run.log");

    let bin = forge_pipeline_bin();
    let out = std::fs::File::create(&log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("run")
        .arg(&file)
        .arg("--project").arg(&project)
        .arg("--workspace").arg(&ws);
    if !body.use_mcp {
        // Default: isolate to the workspace MCP only. When use_mcp is set, run
        // with the project's full .mcp.json so agent nodes reach Gmail/Slack/etc.
        cmd.arg("--isolate-mcp");
    }
    for (k, v) in &body.inputs {
        if !v.trim().is_empty() {
            cmd.arg("--input").arg(format!("{k}={v}"));
        }
    }
    cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
    // Spawn detached; a reaper thread waits so we don't leave a zombie.
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => return Err(AppError::bad_request(format!("could not launch forge-pipeline ({}): {e}", bin.display()))),
    }
    Ok(Json(json!({ "started": true, "log": log.to_string_lossy() })))
}

#[derive(Deserialize)]
pub(crate) struct TeamReqQuery {
    project: String,
    id: String,
}

/// GET /api/team/request?project=NAME&id=req-… — one request's full document plus
/// its response sections (engineer / review / qa).
pub(crate) async fn team_request<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<TeamReqQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    match forge_workspace::request::get_request(&ws, &q.id) {
        Ok(Some((req, res))) => Ok(Json(json!({
            "request": serde_json::to_value(&req).unwrap_or_else(|_| json!({})),
            "response": res.map(|r| serde_json::to_value(&r).unwrap_or_else(|_| json!({}))),
        }))),
        Ok(None) => Err(AppError::not_found("no such request")),
        Err(e) => Err(AppError::from(e)),
    }
}

fn forge_workspace_run_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("forge-workspace-run")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("forge-workspace-run"))
}

/// Is `pid` still alive? (`kill -0`). Unix-only — on Windows this always
/// returns false, so the "team is already running" guard and Stop button are
/// not effective on that platform.
fn pid_alive(pid: &str) -> bool {
    Command::new("kill").arg("-0").arg(pid).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

#[derive(Deserialize)]
pub(crate) struct TeamRun {
    project: String,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    daemon: bool,
}

/// POST /api/team/run — launch the orchestrator (`forge-workspace-run`) against a
/// project (optionally with a --goal for the coordinator, optionally --daemon).
pub(crate) async fn team_run<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TeamRun>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    std::fs::create_dir_all(ws.join("pipelines"))?;
    // Refuse to double-run if one is already alive.
    let pidfile = ws.join(".team-run.pid");
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        if pid_alive(pid.trim()) {
            return Err(AppError::bad_request("the team is already running for this project"));
        }
    }
    // Fresh session: clear the previous run's leftovers so the board's message
    // badges (read/unread) and status reflect THIS run, not stale history.
    // Requests are cleared on a fresh run — their record lived on the previous session.
    if let Ok(entries) = std::fs::read_dir(ws.join("messages")) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = std::fs::remove_file(ws.join(".team-status.json"));
    // Reset the board too: drop old requests so the Done column starts empty
    // for this run (their record lived on the previous session). Requests live
    // as `<ws>/<req-id>/request.yml` directly under the workspace root — only
    // dirs actually containing a request.yml are removed, so pipelines/,
    // messages/, .team-terminal/ etc. are untouched.
    if let Ok(entries) = std::fs::read_dir(&ws) {
        for e in entries.flatten() {
            if e.path().join("request.yml").exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(ws.join(".team-logs")) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let bin = forge_workspace_run_bin();
    let log = std::fs::File::create(ws.join(".team-run.log"))?;
    let err = log.try_clone()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--project").arg(&project).arg("--workspace").arg(&ws);
    if let Some(g) = body.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        cmd.arg("--goal").arg(g);
    }
    if body.daemon {
        cmd.arg("--daemon");
    }
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(err));
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            let _ = std::fs::write(&pidfile, pid.to_string());
            let pf = pidfile.clone();
            std::thread::spawn(move || {
                let mut c = child;
                let _ = c.wait();
                let _ = std::fs::remove_file(&pf);
            });
            Ok(Json(json!({ "started": true, "pid": pid, "daemon": body.daemon })))
        }
        Err(e) => Err(AppError::bad_request(format!("could not launch forge-workspace-run ({}): {e}", bin.display()))),
    }
}

/// GET /api/team/config?project=NAME — the team roster (members + canvas
/// positions). Falls back to the built-in six-role team.
pub(crate) async fn team_config_get<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    let cfg = forge_workspace::team::load_team(&ws);
    Ok(Json(serde_json::to_value(&cfg).unwrap_or_else(|_| json!({}))))
}

#[derive(Deserialize)]
pub(crate) struct TeamConfigSet {
    project: String,
    #[serde(flatten)]
    config: forge_workspace::team::TeamConfig,
}

/// PUT /api/team/config — save the team roster (validated: unique ids, known
/// depends_on, acyclic, at least one implement member).
pub(crate) async fn team_config_set<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TeamConfigSet>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    forge_workspace::team::save_team(&ws, &body.config)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/team/watches?project= — the project's watch list.
pub(crate) async fn team_watches_get<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    let watches = forge_workspace::watch::load_watches(&ws);
    Ok(Json(serde_json::to_value(&watches).unwrap_or_else(|_| json!([]))))
}

#[derive(Deserialize)]
pub(crate) struct WatchesSet {
    project: String,
    watches: Vec<forge_workspace::watch::Watch>,
}

/// PUT /api/team/watches — save the watch list (validated).
pub(crate) async fn team_watches_set<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<WatchesSet>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    forge_workspace::watch::save_watches(&ws, &body.watches)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct CodeQuery {
    project: String,
    #[serde(default)]
    path: String,
}

/// GET /api/team/files?project=&path= — list a project directory (code view).
pub(crate) async fn team_files<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<CodeQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let dir = if q.path.is_empty() { project.clone() } else { project_rel(&project, &q.path)? };
    // Changed paths (repo-relative) from git — marks dirty files/dirs in the tree.
    let changed: Vec<String> = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&project)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.get(3..).map(|p| p.split(" -> ").last().unwrap_or(p).trim_matches('"').to_string()))
                .collect()
        })
        .unwrap_or_default();
    let rel = |name: &str| if q.path.is_empty() { name.to_string() } else { format!("{}/{}", q.path, name) };
    let mut dirs: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let r = rel(&name);
            if e.path().is_dir() {
                let dirty = changed.iter().any(|c| c.starts_with(&format!("{r}/")));
                dirs.push(json!({ "name": name, "changed": dirty }));
            } else {
                files.push(json!({ "name": name, "changed": changed.iter().any(|c| c == &r) }));
            }
        }
    }
    let by_name = |a: &Value, b: &Value| a["name"].as_str().cmp(&b["name"].as_str());
    dirs.sort_by(by_name); files.sort_by(by_name);
    Ok(Json(json!({ "path": q.path, "dirs": dirs, "files": files })))
}

/// GET /api/team/file?project=&path= — read a project file (capped).
pub(crate) async fn team_file<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<CodeQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let mut content = std::fs::read_to_string(project_rel(&project, &q.path)?)
        .map_err(|_| AppError::not_found("not a readable text file"))?;
    if content.len() > 200_000 {
        content.truncate(200_000);
        content.push_str("\n… [truncated]");
    }
    Ok(Json(json!({ "path": q.path, "content": content })))
}

#[derive(Deserialize)]
pub(crate) struct DiffQuery {
    project: String,
    files: String,
}

/// GET /api/team/diff?project=&files=a,b — the code changes for a request's
/// files: uncommitted diff, else the last commit that touched them.
pub(crate) async fn team_diff<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<DiffQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    // Pathspecs after `--`, so no option injection; still, keep them repo-relative
    // and traversal-free for the same reason team_file does (see project_rel).
    let files: Vec<&str> = q
        .files
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty() && !f.contains("..") && !std::path::Path::new(f).is_absolute())
        .collect();
    if files.is_empty() {
        return Ok(Json(json!({ "diff": "" })));
    }
    let run = |args: &[&str]| -> String {
        let mut cmd = Command::new("git");
        cmd.args(args).arg("--").args(&files).current_dir(&project);
        cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    };
    let mut diff = run(&["diff", "HEAD"]);
    if diff.trim().is_empty() {
        diff = run(&["log", "-p", "--max-count=1"]);
    }
    let mut d = diff;
    if d.len() > 60000 { d.truncate(60000); d.push_str("\n… [truncated]"); }
    Ok(Json(json!({ "diff": d })))
}

/// GET /api/team/yaml?project= — the team config as YAML (for export).
pub(crate) async fn team_yaml_get<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let cfg = forge_workspace::team::load_team(&project.join(".forge-workspace"));
    let yaml = serde_yml::to_string(&cfg)
        .map_err(|e| AppError::bad_request(format!("serialize team: {e}")))?;
    Ok(Json(json!({ "yaml": yaml })))
}

#[derive(Deserialize)]
pub(crate) struct TeamYamlSet {
    project: String,
    yaml: String,
}

/// POST /api/team/yaml — import a YAML team config (validated like PUT config).
pub(crate) async fn team_yaml_set<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TeamYamlSet>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let cfg: forge_workspace::team::TeamConfig = serde_yml::from_str(&body.yaml)
        .map_err(|e| AppError::bad_request(format!("bad team YAML: {e}")))?;
    forge_workspace::team::save_team(&project.join(".forge-workspace"), &cfg)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true, "members": cfg.members.len() })))
}

/// GET /api/team/activity?project= — the orchestrator's own progress trace
/// (planning phases, request pickups, completions, stuck alerts), with agent
/// spinner/ANSI noise stripped. This is the "where is it now" feed.
pub(crate) async fn team_activity<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let log_path = project.join(".forge-workspace").join(".team-run.log");
    let file = match std::fs::File::open(&log_path) {
        Ok(f) => f,
        Err(_) => return Ok(Json(json!({ "current": "", "trace": [] }))),
    };
    // ANSI spinner/status tokens to filter out of the activity trace.
    const ACTIVITY_NOISE_TOKENS: &[&str] = &["Forging", "Analyzing", "Ctrl+C", "[2K"];
    // Keep only the orchestrator's meaningful progress lines; drop the forge -p
    // spinner/analysis chatter that gets interleaved. A ring buffer caps memory
    // for long-running daemon sessions.
    const MAX_ACTIVITY_LINES: usize = 40;
    let mut lines: VecDeque<String> = VecDeque::with_capacity(MAX_ACTIVITY_LINES);
    for l in BufReader::new(file).lines().flatten() {
        let t = l.trim_end_matches(|c: char| c.is_control()).trim();
        if t.is_empty() {
            continue;
        }
        let keep = t.contains('⚑')       // planning phase
            || t.contains('→')            // request scheduled to a member
            || t.contains('✓')            // done
            || t.contains('✗')            // failed
            || t.contains("STUCK")
            || t.contains('⏸')            // approval wait
            || t.contains('⏱')            // timeout/retry
            || t.starts_with("▶ orchestrator")
            || t.contains("planning done")
            || t.contains("idle, waiting");
        let noise = ACTIVITY_NOISE_TOKENS.iter().any(|n| t.contains(n));
        if keep && !noise {
            // collapse the ANSI-cleaned line
            let clean: String = t.chars().filter(|c| !c.is_control()).collect();
            if lines.len() >= MAX_ACTIVITY_LINES {
                lines.pop_front();
            }
            lines.push_back(clean);
        }
    }
    let current = lines.back().cloned().unwrap_or_default();
    let recent: Vec<String> = lines.into_iter().collect();
    Ok(Json(json!({ "current": current, "trace": recent })))
}

/// GET /api/team/status?project= — per-member session status (down/idle/
/// working) + whether the orchestrator is alive.
pub(crate) async fn team_status<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    // orchestrator alive?
    let alive = std::fs::read_to_string(ws.join(".team-run.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|pid| {
            std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|st| st.success())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let mut members: Value = std::fs::read_to_string(ws.join(".team-status.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    // Overlay live pause flags: .team-status.json is only written by a
    // running orchestrator, but ⏸/▶ must read truthfully (and be toggleable)
    // while the daemon is down too.
    let paused = forge_workspace::team::load_paused(&ws);
    if let Some(map) = members.as_object_mut() {
        // Every roster member gets an entry even before the orchestrator has
        // ever written status — the ⏸/▶ toggle must exist from the start.
        for m in &forge_workspace::team::load_team(&ws).members {
            map.entry(m.id.clone()).or_insert_with(|| json!({ "status": "idle" }));
        }
        for m in map.values_mut() {
            m["paused"] = json!(false);
        }
        for id in &paused {
            let entry = map.entry(id.clone()).or_insert_with(|| json!({ "status": "paused" }));
            entry["paused"] = json!(true);
            if entry["status"] == json!("idle") {
                entry["status"] = json!("paused");
            }
        }
    }
    Ok(Json(json!({ "running": alive, "members": members })))
}

#[derive(Deserialize)]
pub(crate) struct PauseSet {
    project: String,
    member: String,
    paused: bool,
}

/// POST /api/team/pause — hold (or release) new work for one member. Takes
/// effect at the orchestrator's next scheduling decision; in-flight work
/// finishes normally.
pub(crate) async fn team_pause_set<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<PauseSet>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    forge_workspace::team::set_paused(&ws, &body.member, body.paused)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true, "member": body.member, "paused": body.paused })))
}

#[derive(Deserialize)]
pub(crate) struct SessionLogQuery {
    project: String,
    member: String,
}

/// GET /api/team/session-log?project=&member= — tail of a member's resident
/// session log (its streamed agent output across tasks).
pub(crate) async fn team_session_log<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<SessionLogQuery>,
) -> Result<Json<Value>, AppError> {
    if q.member.contains('/') || q.member.contains("..") {
        return Err(AppError::bad_request("invalid member"));
    }
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let path = project.join(".forge-workspace").join(".team-logs").join(format!("{}.log", q.member));
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    // Tail (last ~16KB) so the response stays small.
    let tail = match log.char_indices().rev().nth(16000) {
        Some((i, _)) => format!("…\n{}", &log[i..]),
        None => log,
    };
    Ok(Json(json!({ "log": tail })))
}

#[derive(Deserialize)]
pub(crate) struct ResetSessionReq {
    project: String,
    member: String,
}

/// POST /api/team/reset-session — drop a member's conversation id so its next
/// run starts with fresh context (and clear its log).
pub(crate) async fn team_reset_session<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<ResetSessionReq>,
) -> Result<Json<Value>, AppError> {
    if body.member.contains('/') || body.member.contains("..") {
        return Err(AppError::bad_request("invalid member"));
    }
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    let sp = ws.join(".team-sessions.json");
    let mut map: serde_json::Map<String, Value> = std::fs::read_to_string(&sp)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.remove(&body.member);
    let _ = std::fs::write(&sp, serde_json::to_string_pretty(&map).unwrap_or_default());
    let _ = std::fs::remove_file(ws.join(".team-logs").join(format!("{}.log", body.member)));
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/team/approvals?project= — pending approval gates.
pub(crate) async fn team_approvals_get<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let map: serde_json::Map<String, Value> =
        std::fs::read_to_string(project.join(".forge-workspace").join(".team-approvals.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let pending: Vec<String> = map
        .iter()
        .filter(|(_, v)| v.as_str() == Some("pending"))
        .map(|(k, _)| k.clone())
        .collect();
    Ok(Json(json!({ "pending": pending })))
}

#[derive(Deserialize)]
pub(crate) struct ApproveReq {
    project: String,
    key: String,
}

/// POST /api/team/approve — grant a pending approval gate.
pub(crate) async fn team_approve<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<ApproveReq>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let path = project.join(".forge-workspace").join(".team-approvals.json");
    let mut map: serde_json::Map<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !map.contains_key(&body.key) {
        return Err(AppError::not_found("no such pending approval"));
    }
    map.insert(body.key.clone(), json!("approved"));
    std::fs::write(&path, serde_json::to_string_pretty(&map).unwrap_or_default())?;
    Ok(Json(json!({ "ok": true })))
}

// ─── Team member templates (global: ~/.forge-web/team-templates.json) ───────
// Mirrors the reference forge's smith templates: save a configured member,
// reuse it in any project's team, export/import as JSON.

fn team_templates_path() -> PathBuf {
    forge_workspace::pipeline::home_dir().join(".forge-web").join("team-templates.json")
}

fn load_team_templates() -> Vec<Value> {
    std::fs::read_to_string(team_templates_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_team_templates(list: &[Value]) {
    if let Some(d) = team_templates_path().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(team_templates_path(), serde_json::to_string_pretty(list).unwrap_or_default());
}

/// GET /api/team/templates — saved member templates.
pub(crate) async fn team_templates_get<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    Json(json!({ "templates": load_team_templates() }))
}

#[derive(Deserialize)]
pub(crate) struct TemplateSave {
    name: String,
    member: Value,
}

/// POST /api/team/templates — upsert a template by name.
pub(crate) async fn team_templates_save<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TemplateSave>,
) -> Result<Json<Value>, AppError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("template name required"));
    }
    let mut list = load_team_templates();
    list.retain(|t| t.get("name").and_then(Value::as_str) != Some(name.as_str()));
    list.push(json!({ "name": name, "member": body.member }));
    save_team_templates(&list);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct TemplateRef {
    name: String,
}

/// POST /api/team/templates/delete
pub(crate) async fn team_templates_delete<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<TemplateRef>,
) -> Json<Value> {
    let mut list = load_team_templates();
    list.retain(|t| t.get("name").and_then(Value::as_str) != Some(q.name.as_str()));
    save_team_templates(&list);
    Json(json!({ "ok": true }))
}

/// POST /api/team/stop — stop the orchestrator running for a project.
pub(crate) async fn team_stop<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let pidfile = project.join(".forge-workspace").join(".team-run.pid");
    let Ok(pid) = std::fs::read_to_string(&pidfile) else {
        return Ok(Json(json!({ "stopped": false, "reason": "not running" })));
    };
    let pid = pid.trim().to_string();
    if pid_alive(&pid) {
        let _ = Command::new("kill").arg(&pid).status();
    }
    let _ = std::fs::remove_file(&pidfile);
    Ok(Json(json!({ "stopped": true })))
}

#[derive(Deserialize)]
pub(crate) struct NodeLogQuery {
    run: String,
    node: String,
}

/// GET /api/pipeline/node-log?run=&node= — a node's live streamed stdout for a
/// run (what the agent/command is printing right now).
pub(crate) async fn node_log<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<NodeLogQuery>,
) -> Result<Json<Value>, AppError> {
    if !forge_workspace::pipeline::validate_pipeline_name(&q.run)
        || !forge_workspace::pipeline::validate_pipeline_name(&q.node)
    {
        return Err(AppError::bad_request("invalid id"));
    }
    let path = runs_ws()
        .join("pipelines")
        .join(".log")
        .join(&q.run)
        .join(format!("{}.txt", q.node));
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(Json(json!({ "log": content })))
}

/// GET /api/team?project=NAME — the resident team's board: work requests (with
/// their pipeline stage + who claimed them) and the message-bus inbox, read from
/// `<project>/.forge-workspace` (what `forge-workspace-run` writes). Pure read.
pub(crate) async fn team_board<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    let reqs = forge_workspace::list_requests(&ws, None).unwrap_or_default();
    let msgs = forge_workspace::list_messages(&ws).unwrap_or_default();
    let requests: Vec<Value> = reqs
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "title": r.title,
                "status": serde_json::to_value(r.status).unwrap_or_else(|_| json!("")),
                "claimed_by": r.claimed_by,
            })
        })
        .collect();
    let messages: Vec<Value> = msgs
        .iter()
        .take(60)
        .map(|m| {
            json!({
                "from": m.from,
                "to": m.to,
                "category": serde_json::to_value(m.category).unwrap_or_else(|_| json!("")),
                "body": m.body,
                "read": m.read,
            })
        })
        .collect();
    let running = std::fs::read_to_string(ws.join(".team-run.pid")).ok().map(|p| pid_alive(p.trim())).unwrap_or(false);
    Ok(Json(json!({ "workspace": ws.to_string_lossy(), "running": running, "requests": requests, "messages": messages })))
}

/// GET /api/pipeline/runs — recent (global) runs with per-node DAG status.
pub(crate) async fn list_runs<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    let runs = forge_workspace::pipeline::list_pipelines(&runs_ws());
    let runs_json: Vec<Value> = runs
        .iter()
        .take(12)
        .map(|p| {
            let nodes: Vec<Value> = p
                .node_order
                .iter()
                .filter_map(|id| p.nodes.get(id).map(|st| (id, st)))
                .map(|(id, st)| {
                    // Include each node's outputs (truncated) so the editor can
                    // show "what did this node produce" after a run.
                    let outputs: serde_json::Map<String, Value> = st
                        .outputs
                        .iter()
                        .map(|(k, v)| {
                            let val = if v.chars().count() > 4000 {
                                format!("{}…", v.chars().take(4000).collect::<String>())
                            } else {
                                v.clone()
                            };
                            (k.clone(), json!(val))
                        })
                        .collect();
                    json!({
                        "id": id,
                        "status": format!("{:?}", st.status).to_lowercase(),
                        "outputs": outputs,
                        "error": st.error,
                    })
                })
                .collect();
            json!({
                "id": p.id,
                "workflow": p.workflow_name,
                "status": format!("{:?}", p.status).to_lowercase(),
                "created_at": p.created_at,
                "nodes": nodes,
            })
        })
        .collect();
    Json(json!({ "runs": runs_json }))
}

/// Expand a leading `~` to the home directory.
fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod path_tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The pipeline name is user input that selects a file to read, write or
    /// delete under ~/.forge-web/pipelines. Traversal here is a write primitive.
    #[test]
    fn test_pipeline_file_rejects_traversal_and_bad_extensions() {
        for bad in [
            "../../.bashrc.yaml",
            "..",
            "sub/dir.yaml",
            "back\\slash.yaml",
            "",
            "   ",
            "no-extension",
            "wrong.json",
            "wrong.yaml.txt",
        ] {
            assert_eq!(
                pipeline_file(bad).is_err(),
                true,
                "pipeline name {bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn test_pipeline_file_accepts_yaml_and_yml() {
        for good in ["build.yaml", "build.yml"] {
            let p = pipeline_file(good).expect("valid name");
            assert_eq!(p.parent(), Some(global_pipelines_dir()).as_deref());
            assert_eq!(p.file_name().and_then(|f| f.to_str()), Some(good));
        }
    }

    /// The layout file sits beside the pipeline, and inherits the same guard.
    #[test]
    fn test_layout_path_sits_beside_the_pipeline_and_inherits_the_guard() {
        let p = layout_path("build.yaml").expect("valid name");
        assert_eq!(p.file_name().and_then(|f| f.to_str()), Some("build.layout.json"));
        assert_eq!(layout_path("../escape.yaml").is_err(), true);
    }

    #[test]
    fn test_validate_reports_bad_yaml_instead_of_panicking() {
        let (ok, err) = validate("this: is: not: valid: yaml:");
        assert_eq!(ok, false);
        assert_eq!(err.is_some(), true);
    }

    #[test]
    fn test_projects_from_tolerates_a_missing_or_malformed_key() {
        assert_eq!(projects_from(&serde_json::json!({})).len(), 0);
        assert_eq!(projects_from(&serde_json::json!({"pipeline_projects": "nope"})).len(), 0);
        assert_eq!(
            projects_from(&serde_json::json!({"pipeline_projects": [{"name": "a", "path": "/p"}]})).len(),
            1
        );
    }

    /// The team code viewer reads whatever file `project_rel` resolves to. An
    /// absolute path must not escape the project: `Path::join` throws the base
    /// away for an absolute argument, so `/etc/passwd` used to read through a
    /// `..`-only check. This is the regression test for that read.
    #[test]
    fn test_project_rel_blocks_escapes() {
        let project = std::path::Path::new("/home/u/proj");

        // The exploit that motivated this: absolute path, no "..".
        assert_eq!(project_rel(project, "/etc/passwd").is_err(), true);
        assert_eq!(project_rel(project, "..").is_err(), true);
        assert_eq!(project_rel(project, "../../etc/passwd").is_err(), true);
        assert_eq!(project_rel(project, "a/../../../etc").is_err(), true);
        assert_eq!(project_rel(project, "").is_err(), true);
        assert_eq!(project_rel(project, "   ").is_err(), true);

        // Legitimate repo-relative paths still resolve, under the project.
        let ok = project_rel(project, "src/main.rs").expect("relative path");
        assert_eq!(ok, std::path::Path::new("/home/u/proj/src/main.rs"));
        assert_eq!(ok.starts_with(project), true);
    }
}
