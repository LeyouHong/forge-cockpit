//! Parallel worktree multi-agent ("squad") runs.
//!
//! Each task gets its own git worktree (isolated checkout) and its own
//! `ForgeAPI` instance (built via the worktree factory). The tasks run
//! concurrently; their events are multiplexed over one SSE stream tagged by
//! task id. Afterwards each worktree's diff can be reviewed, opened as a PR, or
//! discarded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::{Path as AxPath, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use forge_api::API;
use forge_domain::{ChatRequest, ChatResponse, Conversation, Event as DomainEvent};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore, broadcast};

use crate::dto::ChatEventDto;
use crate::{AppError, AppState};

pub(crate) type SquadRegistry = Arc<Mutex<HashMap<String, Arc<SquadRun>>>>;

/// A single task within a run.
pub(crate) struct SquadTask {
    pub id: String,
    pub name: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// "running" | "done" | "failed" | "stopped".
    pub status: Mutex<String>,
    /// Handle to abort the running chat (for the per-task Stop button).
    pub abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

/// A parallel run of several tasks.
pub(crate) struct SquadRun {
    pub base_ref: String,
    pub git_root: PathBuf,
    pub tasks: Vec<Arc<SquadTask>>,
    pub tx: broadcast::Sender<String>,
    pub log: Mutex<Vec<String>>,
    pub seq: AtomicU64,
}

// ---------------------------------------------------------------------------
// git helpers
// ---------------------------------------------------------------------------

fn git(args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn short_id() -> String {
    forge_domain::ConversationId::generate()
        .into_string()
        .chars()
        .take(8)
        .collect()
}

/// Lowercase alnum slug for branch/dir names.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').chars().take(24).collect::<String>();
    if out.is_empty() { "task".to_string() } else { out }
}

// ---------------------------------------------------------------------------
// event fan-out
// ---------------------------------------------------------------------------

/// Tags an event with `task_id` + a monotonic `seq`, logs it (for replay), and
/// broadcasts it. `seq` lets the frontend dedup the replay/live overlap.
async fn emit(run: &SquadRun, task_id: &str, mut ev: Value) {
    let seq = run.seq.fetch_add(1, Ordering::SeqCst);
    if let Value::Object(m) = &mut ev {
        m.insert("task_id".into(), json!(task_id));
        m.insert("seq".into(), json!(seq));
    }
    let s = ev.to_string();
    run.log.lock().await.push(s.clone());
    let _ = run.tx.send(s);
}

async fn set_status(run: &SquadRun, task: &SquadTask, status: &str, extra: Value) {
    *task.status.lock().await = status.to_string();
    let mut ev = json!({ "type": "task_status", "status": status });
    if let (Value::Object(m), Value::Object(e)) = (&mut ev, &extra) {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    emit(run, &task.id, ev).await;

    // Close attached SSE streams once every task is terminal (the sender lives
    // in the registry, so `Closed` never fires on its own).
    let mut all_terminal = true;
    for t in &run.tasks {
        if t.status.lock().await.as_str() == "running" {
            all_terminal = false;
            break;
        }
    }
    if all_terminal {
        let _ = run.tx.send(crate::live::DONE_SENTINEL.to_string());
    }
}

// ---------------------------------------------------------------------------
// POST /api/squad  — start a run
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct TaskInput {
    name: String,
    prompt: String,
}

#[derive(Deserialize)]
pub(crate) struct StartBody {
    #[serde(default)]
    base_ref: Option<String>,
    #[serde(default)]
    concurrency: Option<usize>,
    tasks: Vec<TaskInput>,
}

pub(crate) async fn start_squad<A: API + 'static>(
    State(state): State<AppState<A>>,
    Json(body): Json<StartBody>,
) -> Result<Json<Value>, AppError> {
    if body.tasks.is_empty() {
        return Err(AppError::bad_request("provide at least one task"));
    }
    if body.tasks.len() > 8 {
        return Err(AppError::bad_request("too many tasks (max 8)"));
    }
    let concurrency = body.concurrency.unwrap_or(3).clamp(1, 5);

    let cwd = state.api.environment().cwd;
    let git_root = PathBuf::from(
        git(&["rev-parse", "--show-toplevel"], &cwd)
            .map_err(|e| AppError::bad_request(format!("not a git repo: {e}")))?,
    );
    let base_ref = match body.base_ref {
        Some(b) if !b.trim().is_empty() => b.trim().to_string(),
        _ => git(&["rev-parse", "--abbrev-ref", "HEAD"], &git_root).unwrap_or_else(|_| "HEAD".into()),
    };
    let parent = git_root
        .parent()
        .ok_or_else(|| AppError::bad_request("git repo is at the filesystem root"))?
        .to_path_buf();

    let run_id = short_id();
    let (tx, _rx) = broadcast::channel::<String>(1024);

    // Create the worktrees up front, serialized against other git mutations.
    let mut tasks = Vec::new();
    {
        let _guard = state.config_lock.lock().await;
        for t in &body.tasks {
            let sl = slug(&t.name);
            let branch = format!("squad/{sl}-{run_id}");
            let worktree = parent.join(format!("squad-{sl}-{run_id}"));
            git(
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    worktree.to_str().unwrap_or_default(),
                    &base_ref,
                ],
                &git_root,
            )
            .map_err(|e| AppError::bad_request(format!("worktree add failed: {e}")))?;
            tasks.push(Arc::new(SquadTask {
                id: short_id(),
                name: t.name.clone(),
                branch,
                worktree,
                status: Mutex::new("running".to_string()),
                abort: std::sync::Mutex::new(None),
            }));
        }
    }

    let run = Arc::new(SquadRun {
        base_ref,
        git_root,
        tasks: tasks.clone(),
        tx,
        log: Mutex::new(Vec::new()),
        seq: AtomicU64::new(0),
    });
    state.squads.lock().await.insert(run_id.clone(), run.clone());

    // One concurrent job per task, gated by the semaphore.
    let sem = Arc::new(Semaphore::new(concurrency));
    for (task, input) in tasks.iter().cloned().zip(body.tasks.into_iter()) {
        let api = (state.worktree_factory)(task.worktree.clone());
        let run2 = run.clone();
        let sem2 = sem.clone();
        let prompt = input.prompt;
        let task_ref = task.clone();
        let handle = tokio::spawn(async move {
            let _permit = sem2.acquire_owned().await;
            let conversation = Conversation::generate();
            let conv_id = conversation.id;
            if let Err(e) = api.upsert_conversation(conversation).await {
                set_status(&run2, &task, "failed", json!({ "message": format!("init failed: {e:?}") })).await;
                return;
            }
            let request = ChatRequest::new(DomainEvent::new(prompt), conv_id);
            match api.chat(request).await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(resp) => {
                                // Signal the orchestrator so tool-using turns don't deadlock.
                                if let ChatResponse::ToolCallStart { notifier, .. } = &resp {
                                    notifier.notify_one();
                                }
                                let v = serde_json::to_value(ChatEventDto::from(&resp))
                                    .unwrap_or_else(|_| json!({ "type": "error", "message": "serialize failed" }));
                                emit(&run2, &task.id, v).await;
                            }
                            Err(e) => {
                                emit(&run2, &task.id, json!({ "type": "error", "message": format!("{e:?}") })).await;
                            }
                        }
                    }
                    if let Some(usage) = crate::live::conversation_usage(api.as_ref(), &conv_id).await {
                        emit(&run2, &task.id, usage).await;
                    }
                    set_status(&run2, &task, "done", json!({})).await;
                }
                Err(e) => {
                    set_status(&run2, &task, "failed", json!({ "message": format!("{e:?}") })).await;
                }
            }
        });
        *task_ref.abort.lock().unwrap() = Some(handle.abort_handle());
    }

    Ok(Json(json!({
        "run_id": run_id,
        "concurrency": concurrency,
        "tasks": tasks.iter().map(|t| json!({ "id": t.id, "name": t.name, "branch": t.branch })).collect::<Vec<_>>(),
    })))
}

// ---------------------------------------------------------------------------
// GET /api/squad/{run}/events  — multiplexed SSE
// ---------------------------------------------------------------------------

pub(crate) async fn squad_events<A: API + 'static>(
    State(state): State<AppState<A>>,
    AxPath(run_id): AxPath<String>,
) -> Response {
    let run = match state.squads.lock().await.get(&run_id).cloned() {
        Some(r) => r,
        None => return AppError::not_found("run not found").into_response(),
    };
    let mut rx = run.tx.subscribe();
    let backlog = run.log.lock().await.clone();
    let mut all_terminal = true;
    for t in &run.tasks {
        if t.status.lock().await.as_str() == "running" {
            all_terminal = false;
            break;
        }
    }
    let stream = async_stream::stream! {
        for s in backlog {
            yield Ok::<_, std::convert::Infallible>(SseEvent::default().data(s));
        }
        if !all_terminal {
            loop {
                match rx.recv().await {
                    Ok(s) if s == crate::live::DONE_SENTINEL => break,
                    Ok(s) => yield Ok(SseEvent::default().data(s)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    };
    Sse::new(stream).into_response()
}

// ---------------------------------------------------------------------------
// per-task: diff / pr / discard
// ---------------------------------------------------------------------------

async fn get_run<A>(state: &AppState<A>, id: &str) -> Result<Arc<SquadRun>, AppError> {
    state
        .squads
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| AppError::not_found("run not found"))
}

fn get_task(run: &SquadRun, id: &str) -> Result<Arc<SquadTask>, AppError> {
    run.tasks
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| AppError::not_found("task not found"))
}

/// `GET /api/squad/{run}/{task}/diff` — all changes in the worktree vs base.
pub(crate) async fn squad_diff<A: API>(
    State(state): State<AppState<A>>,
    AxPath((run_id, task_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let run = get_run(&state, &run_id).await?;
    let task = get_task(&run, &task_id)?;
    let wt = task.worktree.to_str().unwrap_or_default().to_string();

    // Stage everything (incl. new files) so the diff captures the full change
    // set, then diff the index against the base ref.
    let _guard = state.config_lock.lock().await;
    let _ = git(&["-C", &wt, "add", "-A"], &run.git_root);
    let diff = git(&["-C", &wt, "diff", "--cached", &run.base_ref], &run.git_root)
        .unwrap_or_default();
    let stat = git(&["-C", &wt, "diff", "--cached", "--stat", &run.base_ref], &run.git_root)
        .unwrap_or_default();
    Ok(Json(json!({ "diff": diff, "stat": stat, "branch": task.branch })))
}

/// `POST /api/squad/{run}/{task}/pr` — commit, push, and open a PR (best-effort).
pub(crate) async fn squad_pr<A: API>(
    State(state): State<AppState<A>>,
    AxPath((run_id, task_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let run = get_run(&state, &run_id).await?;
    let task = get_task(&run, &task_id)?;
    let wt = task.worktree.to_str().unwrap_or_default().to_string();

    let _guard = state.config_lock.lock().await;
    let _ = git(&["-C", &wt, "add", "-A"], &run.git_root);
    // Commit; if there's nothing to commit, report that instead of failing hard.
    if git(&["-C", &wt, "commit", "-m", &format!("squad: {}", task.name)], &run.git_root).is_err() {
        let changed = !git(&["-C", &wt, "status", "--porcelain"], &run.git_root)
            .unwrap_or_default()
            .is_empty();
        if !changed {
            return Ok(Json(json!({ "ok": false, "branch": task.branch, "message": "No changes to commit." })));
        }
    }

    // Best-effort push + PR. Falls back cleanly when there's no pushable remote.
    let pushed = git(&["-C", &wt, "push", "-u", "origin", &task.branch], &run.git_root);
    match pushed {
        Ok(_) => {
            // Try the gh CLI to open the PR; otherwise report the pushed branch.
            let pr = Command::new("gh")
                .args(["pr", "create", "--fill", "--head", &task.branch])
                .current_dir(&run.git_root)
                .output();
            match pr {
                Ok(o) if o.status.success() => Ok(Json(json!({
                    "ok": true, "branch": task.branch,
                    "url": String::from_utf8_lossy(&o.stdout).trim(),
                }))),
                _ => Ok(Json(json!({
                    "ok": true, "branch": task.branch, "pushed": true,
                    "message": format!("Pushed {}. Open the PR on GitHub (gh CLI unavailable).", task.branch),
                }))),
            }
        }
        Err(e) => Ok(Json(json!({
            "ok": true, "branch": task.branch, "pushed": false,
            "message": format!("Committed on branch {}. Push skipped (no remote access): {e}", task.branch),
        }))),
    }
}

/// `POST /api/squad/{run}/{task}/discard` — remove the worktree and branch.
pub(crate) async fn squad_discard<A: API>(
    State(state): State<AppState<A>>,
    AxPath((run_id, task_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let run = get_run(&state, &run_id).await?;
    let task = get_task(&run, &task_id)?;
    let wt = task.worktree.to_str().unwrap_or_default().to_string();

    let _guard = state.config_lock.lock().await;
    let _ = git(&["worktree", "remove", "--force", &wt], &run.git_root);
    let _ = git(&["worktree", "prune"], &run.git_root);
    let _ = git(&["branch", "-D", &task.branch], &run.git_root);
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/squad/{run}/{task}/stop` — abort a running task's chat.
pub(crate) async fn squad_stop<A: API>(
    State(state): State<AppState<A>>,
    AxPath((run_id, task_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let run = get_run(&state, &run_id).await?;
    let task = get_task(&run, &task_id)?;
    // Aborting the tokio task drops the chat stream; the orchestrator's send
    // then fails and the agent turn stops.
    if let Some(h) = task.abort.lock().unwrap().take() {
        h.abort();
    }
    set_status(&run, &task, "stopped", json!({})).await;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// worktree management (registry-independent — survives server restarts)
// ---------------------------------------------------------------------------

fn repo_root<A: API>(state: &AppState<A>) -> anyhow::Result<PathBuf> {
    let cwd = state.api.environment().cwd;
    Ok(PathBuf::from(git(&["rev-parse", "--show-toplevel"], &cwd)?))
}

/// All `squad/*` worktrees currently on disk (parsed from `git worktree list`).
fn scan_worktrees(git_root: &Path) -> Vec<Value> {
    let out = git(&["worktree", "list", "--porcelain"], git_root).unwrap_or_default();
    let mut res = Vec::new();
    let mut path: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let branch = b.trim_start_matches("refs/heads/").to_string();
            if branch.starts_with("squad/") {
                if let Some(p) = &path {
                    res.push(json!({ "path": p, "branch": branch }));
                }
            }
        }
    }
    res
}

/// `GET /api/worktrees` — leftover squad worktrees for cleanup.
pub(crate) async fn list_worktrees_h<A: API>(
    State(state): State<AppState<A>>,
) -> Result<Json<Value>, AppError> {
    let root = repo_root(&state)?;
    Ok(Json(json!({ "worktrees": scan_worktrees(&root) })))
}

#[derive(Deserialize)]
pub(crate) struct PathBody {
    path: String,
}

/// Validates `path` is a real squad worktree, returning its branch.
fn validated_worktree(git_root: &Path, path: &str) -> Result<String, AppError> {
    scan_worktrees(git_root)
        .into_iter()
        .find(|w| w["path"].as_str() == Some(path))
        .and_then(|w| w["branch"].as_str().map(str::to_string))
        .ok_or_else(|| AppError::bad_request("not a known squad worktree"))
}

/// `POST /api/worktrees/diff` — diff a worktree vs its fork point.
pub(crate) async fn worktree_diff<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<PathBody>,
) -> Result<Json<Value>, AppError> {
    let root = repo_root(&state)?;
    let branch = validated_worktree(&root, &body.path)?;
    let base = git(&["merge-base", "HEAD", &branch], &root).unwrap_or_else(|_| "HEAD".to_string());
    let _guard = state.config_lock.lock().await;
    let _ = git(&["-C", &body.path, "add", "-A"], &root);
    let diff = git(&["-C", &body.path, "diff", "--cached", &base], &root).unwrap_or_default();
    let stat = git(&["-C", &body.path, "diff", "--cached", "--stat", &base], &root).unwrap_or_default();
    Ok(Json(json!({ "diff": diff, "stat": stat, "branch": branch })))
}

/// Commits any pending worktree changes, then squash-merges the branch into
/// the main repo's current branch, leaving the result **staged** for the user
/// to review and commit. Rejects when the main repo has uncommitted tracked
/// changes (they would tangle with the merge).
fn apply_branch(git_root: &Path, worktree: &str, branch: &str, label: &str) -> Result<Value, AppError> {
    let _ = git(&["-C", worktree, "add", "-A"], git_root);
    let _ = git(&["-C", worktree, "commit", "-m", &format!("squad: {label}")], git_root);

    let dirty = git(&["status", "--porcelain", "-uno"], git_root).unwrap_or_default();
    if !dirty.is_empty() {
        return Err(AppError::bad_request(
            "current branch has uncommitted changes — commit or stash them first",
        ));
    }
    match git(&["merge", "--squash", branch], git_root) {
        Ok(_) => Ok(json!({
            "ok": true,
            "message": format!("Changes from {branch} are staged on your current branch. Review and commit."),
        })),
        Err(e) => {
            let _ = git(&["reset", "--merge"], git_root);
            Err(AppError::bad_request(format!("merge conflict — apply aborted: {e}")))
        }
    }
}

/// `POST /api/squad/{run}/{task}/apply` — squash the task's changes onto the
/// current branch (staged, not committed).
pub(crate) async fn squad_apply<A: API>(
    State(state): State<AppState<A>>,
    AxPath((run_id, task_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let run = get_run(&state, &run_id).await?;
    let task = get_task(&run, &task_id)?;
    let wt = task.worktree.to_str().unwrap_or_default().to_string();
    let _guard = state.config_lock.lock().await;
    apply_branch(&run.git_root, &wt, &task.branch, &task.name).map(Json)
}

/// `POST /api/worktrees/apply` — same, for leftover worktrees (path-based).
pub(crate) async fn worktree_apply<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<PathBody>,
) -> Result<Json<Value>, AppError> {
    let root = repo_root(&state)?;
    let branch = validated_worktree(&root, &body.path)?;
    let _guard = state.config_lock.lock().await;
    apply_branch(&root, &body.path, &branch, &branch).map(Json)
}

/// `POST /api/worktrees/remove` — remove a squad worktree + branch (validated).
pub(crate) async fn worktree_remove<A: API>(
    State(state): State<AppState<A>>,
    Json(body): Json<PathBody>,
) -> Result<Json<Value>, AppError> {
    let root = repo_root(&state)?;
    let branch = validated_worktree(&root, &body.path)?;
    let _guard = state.config_lock.lock().await;
    let _ = git(&["worktree", "remove", "--force", &body.path], &root);
    let _ = git(&["worktree", "prune"], &root);
    let _ = git(&["branch", "-D", &branch], &root);
    Ok(Json(json!({ "ok": true })))
}
