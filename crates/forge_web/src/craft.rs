//! Crafts — project-scoped mini-apps, aligned with the reference forge.
//!
//! A Craft is a self-contained HTML page living at
//! `<project>/.forge/crafts/<name>.html`. You describe what you want; a
//! background `forge -p` agent writes the file; the Team/Craft view renders it
//! in a sandboxed iframe. Crafts travel with the project (commit them to git).
//!
//! Ours is a single static HTML file (vanilla JS, inline CSS) rather than the
//! reference's React/tsx + SDK — it fits this codebase's zero-build, CSP-locked
//! delivery model.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::extract::{Query, State};
use axum::Json;
use forge_api::API;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{AppError, AppState};

/// Look up a project directory by name from the pipeline projects list.
fn project_path(name: &str) -> Option<PathBuf> {
    crate::pipeline::project_path_pub(name)
}

fn crafts_dir(project: &Path) -> PathBuf {
    project.join(".forge").join("crafts")
}

/// Resolve `<project>/.forge/crafts/<name>.html`, rejecting traversal.
fn craft_file(project: &Path, name: &str) -> Result<PathBuf, AppError> {
    let n = name.trim();
    if n.is_empty() || n.contains('/') || n.contains("..") || n.contains('\\') {
        return Err(AppError::bad_request("invalid craft name"));
    }
    Ok(crafts_dir(project).join(format!("{n}.html")))
}

#[derive(Deserialize)]
pub(crate) struct ProjectQuery {
    project: String,
}

/// GET /api/crafts?project= — list a project's crafts.
pub(crate) async fn list<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(crafts_dir(&project)) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = n.strip_suffix(".html") {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    // A craft is "building" until its file exists and a sibling .prompt shows
    // no in-flight marker; we surface pending markers so the UI can spin.
    let pending: Vec<String> = std::fs::read_dir(crafts_dir(&project))
        .map(|es| {
            es.flatten()
                .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".building").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({ "crafts": names, "building": pending })))
}

#[derive(Deserialize)]
pub(crate) struct CraftRef {
    project: String,
    name: String,
}

/// GET /api/craft?project=&name= — a craft's HTML (raw, for the iframe srcdoc).
pub(crate) async fn read<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<CraftRef>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let file = craft_file(&project, &q.name)?;
    let content = std::fs::read_to_string(&file).map_err(|_| AppError::not_found("no such craft"))?;
    let prompt = std::fs::read_to_string(file.with_extension("prompt")).unwrap_or_default();
    Ok(Json(json!({ "name": q.name, "html": content, "prompt": prompt })))
}

/// POST /api/craft/delete
pub(crate) async fn delete<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<CraftRef>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&q.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let file = craft_file(&project, &q.name)?;
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(file.with_extension("prompt"));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct GenerateReq {
    project: String,
    name: String,
    prompt: String,
    /// When true, refine the existing craft instead of creating fresh.
    #[serde(default)]
    refine: bool,
}

/// The instructions the craft-builder agent follows to emit one self-contained
/// HTML file. Kept strict so the output renders in a sandboxed iframe.
fn builder_prompt(file: &Path, project: &Path, user: &str, refine: bool, prior: &str) -> String {
    let mode = if refine {
        format!(
            "You are REFINING an existing mini-app. Its current HTML is below; apply the change and \
             rewrite the whole file.\n\n--- CURRENT ---\n{prior}\n--- END CURRENT ---\n\n"
        )
    } else {
        String::new()
    };
    format!(
        "You are a Craft builder. Create a single self-contained HTML mini-app for this project and \
         write it to `{file}` (overwrite it). {mode}\
         \n## The request\n{user}\n\
         \n## Hard requirements\n\
         - Output ONE `.html` file: inline all CSS in a <style> tag and all JS in a <script> tag. \
         NO external URLs (scripts, stylesheets, fonts, images) — it renders in a sandboxed iframe \
         with no network.\n\
         - It must be useful and interactive on its own. If it needs project data, you MAY read \
         files under `{project}` NOW (with your tools) and BAKE the data into the HTML as a JS \
         constant — the running page has no server.\n\
         - Clean, modern, responsive; dark-friendly. No build step, no frameworks via CDN.\n\
         - Do not write any file other than `{file}`. When done, reply with one short sentence.",
        file = file.display(),
        project = project.display(),
        mode = mode,
        user = user,
    )
}

/// POST /api/craft/generate — spawn a background agent to write the craft file.
pub(crate) async fn generate<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<GenerateReq>,
) -> Result<Json<Value>, AppError> {
    let project = project_path(&body.project).ok_or_else(|| AppError::not_found("no such project"))?;
    let file = craft_file(&project, &body.name)?;
    if body.prompt.trim().is_empty() {
        return Err(AppError::bad_request("describe what the craft should do"));
    }
    std::fs::create_dir_all(crafts_dir(&project))?;

    let prior = if body.refine {
        std::fs::read_to_string(&file).unwrap_or_default()
    } else {
        String::new()
    };
    let prompt = builder_prompt(&file, &project, &body.prompt, body.refine, &prior);

    // Record the request (+ history) next to the craft, and a .building marker.
    let prompt_log = file.with_extension("prompt");
    let mut hist = std::fs::read_to_string(&prompt_log).unwrap_or_default();
    hist.push_str(&format!("- {}\n", body.prompt.replace('\n', " ")));
    let _ = std::fs::write(&prompt_log, &hist);
    let marker = crafts_dir(&project).join(format!("{}.building", body.name));
    let _ = std::fs::write(&marker, "");

    let exe = std::env::current_exe().ok();
    let forge = exe
        .as_ref()
        .and_then(|e| e.parent())
        .map(|d| d.join("forge"))
        .unwrap_or_else(|| PathBuf::from("forge"));

    let mut cmd = Command::new(forge);
    cmd.arg("-p")
        .arg(&prompt)
        .current_dir(&project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            // Reaper thread clears the .building marker when the agent exits.
            std::thread::spawn(move || {
                let _ = child.wait();
                let _ = std::fs::remove_file(&marker);
            });
            Ok(Json(json!({ "started": true })))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&marker);
            Err(AppError::bad_request(format!("failed to start builder: {e}")))
        }
    }
}
