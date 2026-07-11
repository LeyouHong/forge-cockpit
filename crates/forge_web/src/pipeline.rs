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

use std::path::{Path, PathBuf};
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
    // Make sure the pipelines dir exists so the first "New" lands somewhere.
    let _ = std::fs::create_dir_all(pipelines_dir(&path));
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

// ─── Pipeline files (<project>/.forge/pipelines/*.yaml) ─────────────────────

fn pipelines_dir(project: &Path) -> PathBuf {
    project.join(".forge").join("pipelines")
}

/// Resolve `<project>/.forge/pipelines/<name>`, rejecting path traversal.
fn pipeline_file(project_name: &str, file: &str) -> Result<PathBuf, AppError> {
    if file.contains('/') || file.contains("..") || file.trim().is_empty() {
        return Err(AppError::bad_request("invalid pipeline name"));
    }
    if !(file.ends_with(".yaml") || file.ends_with(".yml")) {
        return Err(AppError::bad_request("pipeline name must end in .yaml/.yml"));
    }
    let path = project_path(project_name).ok_or_else(|| AppError::not_found("no such project"))?;
    Ok(pipelines_dir(&path).join(file))
}

#[derive(Deserialize)]
pub(crate) struct ProjectQuery {
    project: String,
}

/// GET /api/pipeline/files?project=NAME — list workflow files in the project.
pub(crate) async fn list_files<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let path = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let dir = pipelines_dir(&path);
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
    Ok(Json(json!({ "dir": dir.to_string_lossy(), "files": files })))
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    project: String,
    name: String,
}

/// GET /api/pipeline/file?project=NAME&name=FILE — read a workflow + validation.
pub(crate) async fn read_file<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&q.project, &q.name)?;
    let content = std::fs::read_to_string(&path).map_err(|_| AppError::not_found("no such pipeline"))?;
    let (valid, error) = validate(&content);
    Ok(Json(json!({ "name": q.name, "content": content, "valid": valid, "error": error })))
}

#[derive(Deserialize)]
pub(crate) struct FileSave {
    project: String,
    name: String,
    content: String,
}

/// PUT /api/pipeline/file — save (creating if new). Always saves; reports the
/// validation result so the editor can show it without losing work.
pub(crate) async fn save_file<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<FileSave>,
) -> Result<Json<Value>, AppError> {
    let path = pipeline_file(&body.project, &body.name)?;
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
    let path = pipeline_file(&q.project, &q.name)?;
    std::fs::remove_file(&path).map_err(|_| AppError::not_found("no such pipeline"))?;
    Ok(Json(json!({ "ok": true })))
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

/// POST /api/pipeline/run — run a workflow against its project (background).
pub(crate) async fn run_pipeline<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<FileQuery>,
) -> Result<Json<Value>, AppError> {
    let file = pipeline_file(&q.project, &q.name)?;
    if !file.exists() {
        return Err(AppError::not_found("no such pipeline"));
    }
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
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
        .arg("--isolate-mcp")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
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

/// GET /api/pipeline/runs?project=NAME — recent runs with per-node DAG status.
pub(crate) async fn list_runs<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let ws = project.join(".forge-workspace");
    let runs = forge_workspace::pipeline::list_pipelines(&ws);
    let runs_json: Vec<Value> = runs
        .iter()
        .take(12)
        .map(|p| {
            let nodes: Vec<Value> = p
                .node_order
                .iter()
                .filter_map(|id| p.nodes.get(id).map(|st| (id, st)))
                .map(|(id, st)| json!({ "id": id, "status": format!("{:?}", st.status).to_lowercase() }))
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
    Ok(Json(json!({ "runs": runs_json })))
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
