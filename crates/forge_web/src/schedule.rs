//! Schedules — timed automation, aligned with the reference forge:
//! **schedule = trigger + body**. The trigger decides *when* to fire
//! (every N minutes / cron / once / manual); the body decides *what* runs —
//! a saved global pipeline with inputs, or a one-shot `forge -p` prompt.
//!
//! Storage is file-based like everything else here:
//!   ~/.forge-web/schedules.json       configuration
//!   ~/.forge-web/schedule-runs.json   fire history (capped)
//!
//! A 30s tick loop (spawned at server start) fires due schedules. A schedule
//! with an inflight run is skipped (no pile-up); its `next_run_at` still
//! advances. No retry/dedup logic at this level — that's the body's job.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Local, Utc};
use forge_api::API;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{AppError, AppState};

fn web_home() -> PathBuf {
    forge_workspace::pipeline::home_dir().join(".forge-web")
}
fn schedules_path() -> PathBuf {
    web_home().join("schedules.json")
}
fn runs_path() -> PathBuf {
    web_home().join("schedule-runs.json")
}

/// Single lock for both files — fires and API edits are rare and cheap.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// "pipeline" | "prompt"
    pub body_kind: String,
    /// Pipeline file name (body_kind=pipeline).
    #[serde(default)]
    pub pipeline: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// Working directory for the body (shell-expanded; empty → home).
    #[serde(default)]
    pub dir: String,
    /// One-shot instructions (body_kind=prompt).
    #[serde(default)]
    pub prompt: String,
    /// Optional forge agent for prompt bodies (`--agent`).
    #[serde(default)]
    pub agent: String,
    /// "every" | "cron" | "once" | "manual"
    pub trigger: String,
    #[serde(default)]
    pub every_minutes: u64,
    #[serde(default)]
    pub cron: String,
    /// RFC3339 local time for `once`.
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRun {
    pub schedule_id: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// "started" | "done" | "failed"
    pub status: String,
    /// "manual" | "timer"
    pub fired_by: String,
    /// Tail of the body's output (for the runs view).
    #[serde(default)]
    pub output_tail: String,
}

fn load_schedules() -> Vec<Schedule> {
    std::fs::read_to_string(schedules_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_schedules(list: &[Schedule]) {
    let _ = std::fs::create_dir_all(web_home());
    let _ = std::fs::write(schedules_path(), serde_json::to_string_pretty(list).unwrap_or_default());
}
fn load_runs() -> Vec<ScheduleRun> {
    std::fs::read_to_string(runs_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_runs(runs: &[ScheduleRun]) {
    let _ = std::fs::create_dir_all(web_home());
    // Cap history so the file can't grow without bound.
    let start = runs.len().saturating_sub(300);
    let _ = std::fs::write(runs_path(), serde_json::to_string_pretty(&runs[start..]).unwrap_or_default());
}

/// A 5-field cron is the common form; the `cron` crate wants seconds first.
fn normalize_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 { format!("0 {expr}") } else { expr.to_string() }
}

/// When this schedule should fire next, computed *after* `from`. Cron fields
/// are interpreted in local time.
fn compute_next(s: &Schedule, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match s.trigger.as_str() {
        "every" => Some(from + chrono::Duration::minutes(s.every_minutes.max(1) as i64)),
        "cron" => {
            let sched = cron::Schedule::from_str(&normalize_cron(&s.cron)).ok()?;
            sched.after(&from.with_timezone(&Local)).next().map(|t| t.with_timezone(&Utc))
        }
        "once" => {
            let at = DateTime::parse_from_rfc3339(&s.at)
                .ok()
                .map(|t| t.with_timezone(&Utc))
                .or_else(|| {
                    chrono::NaiveDateTime::parse_from_str(&s.at, "%Y-%m-%dT%H:%M")
                        .ok()
                        .and_then(|n| n.and_local_timezone(Local).single())
                        .map(|t| t.with_timezone(&Utc))
                })?;
            (at > from).then_some(at)
        }
        _ => None, // manual
    }
}

fn validate(s: &Schedule) -> Result<(), String> {
    if s.name.trim().is_empty() {
        return Err("name required".into());
    }
    match s.body_kind.as_str() {
        "pipeline" => {
            if s.pipeline.trim().is_empty() {
                return Err("pick a pipeline".into());
            }
            let f = forge_workspace::pipeline::global_pipelines_dir().join(&s.pipeline);
            if !f.exists() {
                return Err(format!("no such pipeline `{}`", s.pipeline));
            }
        }
        "prompt" => {
            if s.prompt.trim().is_empty() {
                return Err("prompt required".into());
            }
        }
        other => return Err(format!("unknown body kind `{other}`")),
    }
    match s.trigger.as_str() {
        "every" => {
            if s.every_minutes == 0 {
                return Err("interval must be ≥ 1 minute".into());
            }
        }
        "cron" => {
            cron::Schedule::from_str(&normalize_cron(&s.cron))
                .map_err(|e| format!("bad cron expression: {e}"))?;
        }
        "once" => {
            if compute_next(&Schedule { at: s.at.clone(), trigger: "once".into(), ..Default::default() }, Utc::now()).is_none() {
                return Err("`once` time must parse and be in the future".into());
            }
        }
        "manual" => {}
        other => return Err(format!("unknown trigger `{other}`")),
    }
    Ok(())
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        return forge_workspace::pipeline::home_dir().join(rest).to_string_lossy().to_string();
    }
    p.to_string()
}

fn exe_sibling(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn tail(s: &str, n: usize) -> String {
    match s.char_indices().rev().nth(n.saturating_sub(1)) {
        Some((i, _)) => s[i..].to_string(),
        None => s.to_string(),
    }
}

/// Fire a schedule's body on a blocking thread; records a run and updates it
/// when the body exits. Returns immediately.
fn fire(s: Schedule, fired_by: &str) {
    let started = Utc::now();
    {
        let _g = LOCK.lock().unwrap();
        let mut runs = load_runs();
        runs.push(ScheduleRun {
            schedule_id: s.id.clone(),
            started_at: started,
            finished_at: None,
            status: "started".into(),
            fired_by: fired_by.into(),
            output_tail: String::new(),
        });
        save_runs(&runs);
    }
    std::thread::spawn(move || {
        let dir = if s.dir.trim().is_empty() {
            forge_workspace::pipeline::home_dir()
        } else {
            PathBuf::from(shellexpand(&s.dir))
        };
        let (status, out) = match s.body_kind.as_str() {
            "pipeline" => {
                let file = forge_workspace::pipeline::global_pipelines_dir().join(&s.pipeline);
                let ws = forge_workspace::pipeline::global_runs_workspace();
                let mut cmd = Command::new(exe_sibling("forge-pipeline"));
                cmd.arg("run").arg(&file).arg("--project").arg(&dir).arg("--workspace").arg(&ws).arg("--isolate-mcp");
                for (k, v) in &s.inputs {
                    if !v.trim().is_empty() {
                        cmd.arg("--input").arg(format!("{k}={v}"));
                    }
                }
                run_body(cmd)
            }
            _ => {
                let mut cmd = Command::new(exe_sibling("forge"));
                cmd.arg("-p").arg(&s.prompt).current_dir(&dir);
                if !s.agent.trim().is_empty() {
                    cmd.arg("--agent").arg(&s.agent);
                }
                run_body(cmd)
            }
        };
        let _g = LOCK.lock().unwrap();
        let mut runs = load_runs();
        if let Some(r) = runs
            .iter_mut()
            .rev()
            .find(|r| r.schedule_id == s.id && r.status == "started" && r.started_at == started)
        {
            r.finished_at = Some(Utc::now());
            r.status = if status { "done".into() } else { "failed".into() };
            r.output_tail = tail(&out, 2000);
        }
        save_runs(&runs);
    });
}

/// Run a body command to completion (2h cap), returning (success, output).
fn run_body(mut cmd: Command) -> (bool, String) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("spawn failed: {e}")),
    };
    // wait_with_output has no timeout; bodies are killed by their own layers
    // (pipeline node timeouts, forge session limits). 2h is a safety net.
    let handle = std::thread::spawn(move || child.wait_with_output());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2 * 60 * 60);
    loop {
        if handle.is_finished() {
            return match handle.join() {
                Ok(Ok(o)) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    let err = String::from_utf8_lossy(&o.stderr);
                    if !err.trim().is_empty() {
                        text.push_str("\n--- stderr ---\n");
                        text.push_str(err.trim_end());
                    }
                    (o.status.success(), text)
                }
                Ok(Err(e)) => (false, format!("wait failed: {e}")),
                Err(_) => (false, "body thread panicked".into()),
            };
        }
        if std::time::Instant::now() >= deadline {
            return (false, "timed out after 2h (body still running, orphaned)".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// The scheduler tick — call from a spawned tokio task at server start.
pub async fn tick_loop() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let now = Utc::now();
        let (due, stale): (Vec<Schedule>, bool) = {
            let _g = LOCK.lock().unwrap();
            let mut list = load_schedules();
            let runs = load_runs();
            let mut due = Vec::new();
            let mut changed = false;
            for s in list.iter_mut() {
                if !s.enabled || s.trigger == "manual" {
                    continue;
                }
                if s.next_run_at.is_none() {
                    s.next_run_at = compute_next(s, now);
                    changed = true;
                    continue;
                }
                if s.next_run_at.is_some_and(|t| t <= now) {
                    let inflight = runs.iter().any(|r| r.schedule_id == s.id && r.status == "started");
                    if !inflight {
                        due.push(s.clone());
                    }
                    // Advance regardless — a busy schedule skips, not queues.
                    s.next_run_at = compute_next(s, now);
                    if s.trigger == "once" {
                        s.enabled = false; // single shot
                    }
                    changed = true;
                }
            }
            // Reconcile-lite: a "started" run older than 24h is a lie (server
            // restarted mid-body) — mark it failed so state isn't stuck.
            let mut runs = runs;
            let mut runs_changed = false;
            for r in runs.iter_mut() {
                if r.status == "started" && now - r.started_at > chrono::Duration::hours(24) {
                    r.status = "failed".into();
                    r.finished_at = Some(now);
                    r.output_tail = "orphaned (server restarted?)".into();
                    runs_changed = true;
                }
            }
            if runs_changed {
                save_runs(&runs);
            }
            if changed {
                save_schedules(&list);
            }
            (due, runs_changed)
        };
        let _ = stale;
        for s in due {
            fire(s, "timer");
        }
    }
}

// ─── API ─────────────────────────────────────────────────────────────────────

fn decorate(s: &Schedule, runs: &[ScheduleRun]) -> Value {
    let mine: Vec<&ScheduleRun> = runs.iter().filter(|r| r.schedule_id == s.id).collect();
    let inflight = mine.iter().any(|r| r.status == "started");
    let last = mine.iter().rev().find(|r| r.status != "started");
    let state = if !s.enabled {
        "paused"
    } else if inflight {
        "running"
    } else if last.is_some_and(|r| r.status == "failed") {
        "last_failed"
    } else {
        "idle"
    };
    let mut v = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
    v["state"] = json!(state);
    v["last_run"] = last.map(|r| json!({ "at": r.started_at, "status": r.status })).unwrap_or(Value::Null);
    v
}

/// GET /api/schedules
pub(crate) async fn list<A: API>(State(_): State<AppState<A>>) -> Json<Value> {
    let _g = LOCK.lock().unwrap();
    let runs = load_runs();
    let out: Vec<Value> = load_schedules().iter().map(|s| decorate(s, &runs)).collect();
    Json(json!({ "schedules": out }))
}

/// POST /api/schedules — create (body = Schedule fields sans id).
pub(crate) async fn create<A: API>(
    State(_): State<AppState<A>>,
    Json(mut body): Json<Schedule>,
) -> Result<Json<Value>, AppError> {
    body.id = format!("sch-{:08x}", rand_id());
    body.created_at = Utc::now().to_rfc3339();
    body.next_run_at = None;
    validate(&body).map_err(AppError::bad_request)?;
    body.next_run_at = compute_next(&body, Utc::now());
    let _g = LOCK.lock().unwrap();
    let mut list = load_schedules();
    list.push(body.clone());
    save_schedules(&list);
    Ok(Json(json!({ "ok": true, "id": body.id })))
}

fn rand_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 ^ (std::process::id() as u64) << 32)
        .unwrap_or_default()
}

#[derive(Deserialize)]
pub(crate) struct IdRef {
    id: String,
}

/// POST /api/schedules/update — full replace of editable fields.
pub(crate) async fn update<A: API>(
    State(_): State<AppState<A>>,
    Json(body): Json<Schedule>,
) -> Result<Json<Value>, AppError> {
    validate(&body).map_err(AppError::bad_request)?;
    let _g = LOCK.lock().unwrap();
    let mut list = load_schedules();
    let Some(cur) = list.iter_mut().find(|s| s.id == body.id) else {
        return Err(AppError::not_found("no such schedule"));
    };
    let created = cur.created_at.clone();
    *cur = body;
    cur.created_at = created;
    cur.next_run_at = compute_next(cur, Utc::now());
    save_schedules(&list);
    Ok(Json(json!({ "ok": true })))
}

/// POST /api/schedules/delete
pub(crate) async fn delete<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<IdRef>,
) -> Json<Value> {
    let _g = LOCK.lock().unwrap();
    let mut list = load_schedules();
    list.retain(|s| s.id != q.id);
    save_schedules(&list);
    Json(json!({ "ok": true }))
}

/// POST /api/schedules/fire — run now (works for manual + paused too).
pub(crate) async fn fire_now<A: API>(
    State(_): State<AppState<A>>,
    Json(q): Json<IdRef>,
) -> Result<Json<Value>, AppError> {
    let s = {
        let _g = LOCK.lock().unwrap();
        load_schedules().into_iter().find(|s| s.id == q.id)
    }
    .ok_or_else(|| AppError::not_found("no such schedule"))?;
    fire(s, "manual");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct RunsQuery {
    id: String,
}

/// GET /api/schedule-runs?id= — newest first.
pub(crate) async fn runs<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<RunsQuery>,
) -> Json<Value> {
    let _g = LOCK.lock().unwrap();
    let mut mine: Vec<ScheduleRun> = load_runs().into_iter().filter(|r| r.schedule_id == q.id).collect();
    mine.reverse();
    mine.truncate(20);
    Json(json!({ "runs": mine }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_next_triggers() {
        let now = Utc::now();
        let every = Schedule { trigger: "every".into(), every_minutes: 15, ..Default::default() };
        assert_eq!(compute_next(&every, now).unwrap(), now + chrono::Duration::minutes(15));

        let cron5 = Schedule { trigger: "cron".into(), cron: "0 9 * * 1-5".into(), ..Default::default() };
        let next = compute_next(&cron5, now).unwrap();
        assert!(next > now);

        let past = Schedule { trigger: "once".into(), at: "2000-01-01T00:00:00Z".into(), ..Default::default() };
        assert!(compute_next(&past, now).is_none());

        let manual = Schedule { trigger: "manual".into(), ..Default::default() };
        assert!(compute_next(&manual, now).is_none());
    }

    #[test]
    fn test_validate() {
        let ok = Schedule {
            name: "n".into(),
            body_kind: "prompt".into(),
            prompt: "do things".into(),
            trigger: "every".into(),
            every_minutes: 5,
            ..Default::default()
        };
        assert!(validate(&ok).is_ok());
        assert!(validate(&Schedule { name: "".into(), ..ok.clone() }).is_err());
        assert!(validate(&Schedule { trigger: "cron".into(), cron: "not a cron".into(), ..ok.clone() }).is_err());
        assert!(validate(&Schedule { body_kind: "pipeline".into(), pipeline: "missing.yaml".into(), ..ok.clone() }).is_err());
        assert!(validate(&Schedule { every_minutes: 0, ..ok }).is_err());
    }
}
