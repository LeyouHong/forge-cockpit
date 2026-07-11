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
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
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

fn web_home() -> PathBuf {
    home_dir().join(".forge-web")
}
/// Where global pipeline files live.
fn global_pipelines_dir() -> PathBuf {
    web_home().join("pipelines")
}
/// The workspace all runs persist to (global, not per project).
fn runs_ws() -> PathBuf {
    web_home().join("runs")
}

/// Resolve `~/.forge-web/pipelines/<name>`, rejecting path traversal.
fn pipeline_file(file: &str) -> Result<PathBuf, AppError> {
    if file.contains('/') || file.contains("..") || file.trim().is_empty() {
        return Err(AppError::bad_request("invalid pipeline name"));
    }
    if !(file.ends_with(".yaml") || file.ends_with(".yml")) {
        return Err(AppError::bad_request("pipeline name must end in .yaml/.yml"));
    }
    Ok(global_pipelines_dir().join(file))
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
        .arg("--workspace").arg(&ws)
        .arg("--isolate-mcp");
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

/// Is `pid` still alive? (`kill -0`)
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
    let bin = forge_workspace_run_bin();
    let log = std::fs::File::create(ws.join(".team-run.log"))?;
    let err = log.try_clone()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--project").arg(&project).arg("--workspace").arg(&ws).arg("--isolate-mcp");
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
            std::thread::spawn(move || {
                let mut c = child;
                let _ = c.wait();
            });
            Ok(Json(json!({ "started": true, "pid": pid, "daemon": body.daemon })))
        }
        Err(e) => Err(AppError::bad_request(format!("could not launch forge-workspace-run ({}): {e}", bin.display()))),
    }
}

/// GET /api/team/agents?project=NAME — the role→agent map for this project.
pub(crate) async fn team_agents_get<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let map: Value = std::fs::read_to_string(project.join(".forge-workspace").join(".team-agents.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({ "agents": map })))
}

#[derive(Deserialize)]
pub(crate) struct TeamAgentsSet {
    project: String,
    agents: Value,
}

/// PUT /api/team/agents — save the role→agent map (used by the orchestrator to
/// pass `--agent` per role). Empty values are dropped.
pub(crate) async fn team_agents_set<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<TeamAgentsSet>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    std::fs::create_dir_all(&ws)?;
    let clean: serde_json::Map<String, Value> = body
        .agents
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    std::fs::write(ws.join(".team-agents.json"), serde_json::to_string_pretty(&clean).unwrap_or_default())?;
    Ok(Json(json!({ "ok": true })))
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
    if [&q.run, &q.node].iter().any(|s| s.contains('/') || s.contains("..")) {
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
