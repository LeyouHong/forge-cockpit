//! The orchestrator — the resident "team brain".
//!
//! An event-driven loop over the workspace: for every pending request it spawns
//! a `forge` subprocess acting as the right role (role SOP as prompt, workspace
//! MCP connected). Agents drive status forward through the MCP tools; the
//! orchestrator re-reads state and reacts. Compared to a one-shot runner it adds:
//!
//!   - concurrency     — several requests worked in parallel (--concurrent N)
//!   - stuck detection — if a request sits in one status for too many attempts,
//!                       it is marked stuck and left alone (no infinite token burn)
//!   - daemon mode     — keep running and pick up new requests (--daemon)
//!
//!   forge-workspace-run --project DIR [--workspace DIR] [--forge PATH]
//!       [--concurrent N] [--max-attempts N] [--poll-secs N] [--daemon] [--dry-run]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_workspace::request::{self, RequestDocument, RequestStatus};
use serde_json::json;

const ENGINEER_SOP: &str = include_str!("../../roles/engineer.md");
const REVIEWER_SOP: &str = include_str!("../../roles/reviewer.md");
const QA_SOP: &str = include_str!("../../roles/qa.md");

fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    if let Some(i) = args.iter().position(|a| a == name) {
        if i + 1 < args.len() {
            let v = args.remove(i + 1);
            args.remove(i);
            return Some(v);
        }
    }
    None
}
fn has(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        true
    } else {
        false
    }
}

/// Per-request progress tracker for stuck detection.
#[derive(Default)]
struct Tracker {
    last_status: Option<RequestStatus>,
    attempts: u32,
    stuck: bool,
}

struct State {
    running: HashSet<String>,
    trackers: HashMap<String, Tracker>,
}

/// Outcome of the tracker update for one request this poll.
enum Decision {
    Skip,
    Stuck,
    Run(u32),
}

struct Cfg {
    project: PathBuf,
    workspace: PathBuf,
    forge: PathBuf,
    concurrent: usize,
    max_attempts: u32,
    poll: Duration,
    daemon: bool,
    dry_run: bool,
    /// When set (via --isolate-mcp), spawned agents run with FORGE_CONFIG=this,
    /// an isolated base_path exposing ONLY the workspace MCP.
    home: Option<PathBuf>,
}

fn pending(r: &RequestDocument) -> bool {
    matches!(
        r.status,
        RequestStatus::Open | RequestStatus::InProgress | RequestStatus::Review | RequestStatus::Qa
    )
}

fn role_for(status: RequestStatus) -> (&'static str, &'static str) {
    match status {
        RequestStatus::Open | RequestStatus::InProgress => ("engineer", ENGINEER_SOP),
        RequestStatus::Review => ("reviewer", REVIEWER_SOP),
        RequestStatus::Qa => ("qa", QA_SOP),
        _ => ("engineer", ENGINEER_SOP),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("orchestrator error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let project = flag(&mut args, "--project").map(PathBuf::from).unwrap_or_else(|| ".".into()).canonicalize()?;
    let workspace = flag(&mut args, "--workspace").map(PathBuf::from).unwrap_or_else(|| project.join(".forge-workspace"));
    std::fs::create_dir_all(&workspace)?;
    let workspace = workspace.canonicalize()?;
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
    let forge = flag(&mut args, "--forge").map(PathBuf::from).unwrap_or_else(|| exe_dir.join("forge"));
    let mcp_bin = exe_dir.join("forge-workspace-mcp");

    let isolate = has(&mut args, "--isolate-mcp");
    let home = if isolate { Some(setup_isolated_home(&workspace, &mcp_bin)?) } else { None };
    let cfg = Cfg {
        project,
        workspace,
        forge,
        concurrent: flag(&mut args, "--concurrent").and_then(|s| s.parse().ok()).unwrap_or(1).max(1),
        max_attempts: flag(&mut args, "--max-attempts").and_then(|s| s.parse().ok()).unwrap_or(4),
        poll: Duration::from_secs(flag(&mut args, "--poll-secs").and_then(|s| s.parse().ok()).unwrap_or(3)),
        daemon: has(&mut args, "--daemon"),
        dry_run: has(&mut args, "--dry-run"),
        home,
    };

    // With an isolated base_path the workspace MCP is provided there; otherwise
    // inject it into the project's .mcp.json (which merges with the global one).
    if cfg.home.is_none() {
        ensure_workspace_mcp(&cfg.project, &mcp_bin, &cfg.workspace)?;
    }
    println!(
        "▶ orchestrator: project={} concurrent={} max-attempts={} daemon={} dry-run={} isolate-mcp={}",
        cfg.project.display(), cfg.concurrent, cfg.max_attempts, cfg.daemon, cfg.dry_run, cfg.home.is_some()
    );

    let state = Arc::new(Mutex::new(State { running: HashSet::new(), trackers: HashMap::new() }));
    let cfg = Arc::new(cfg);
    let mut idle_since: Option<Instant> = None;

    loop {
        let reqs = request::list_requests(&cfg.workspace, None)?;
        let todo: Vec<RequestDocument> = reqs.into_iter().filter(pending).collect();

        // Schedule pending requests that aren't already running / stuck.
        {
            let mut guard = state.lock().unwrap();
            let st = &mut *guard; // reborrow so trackers/running can be split-borrowed
            for req in &todo {
                if st.running.contains(&req.id) {
                    continue;
                }
                // Update the tracker in a small scope so its &mut ends before we
                // touch `running`. Progress = status changed since last look.
                let decision = {
                    let t = st.trackers.entry(req.id.clone()).or_default();
                    if t.stuck {
                        Decision::Skip
                    } else {
                        if t.last_status == Some(req.status) {
                            t.attempts += 1;
                        } else {
                            t.last_status = Some(req.status);
                            t.attempts = 1;
                        }
                        if t.attempts > cfg.max_attempts {
                            t.stuck = true;
                            Decision::Stuck
                        } else {
                            Decision::Run(t.attempts)
                        }
                    }
                };
                match decision {
                    Decision::Skip => continue,
                    Decision::Stuck => {
                        println!("  ✗ {} STUCK in {:?} after {} attempts — leaving it.", req.id, req.status, cfg.max_attempts);
                        continue;
                    }
                    Decision::Run(attempts) => {
                        if st.running.len() >= cfg.concurrent {
                            continue; // saturated; try next poll
                        }
                        let (role, sop) = role_for(req.status);
                        println!("  → {} [{:?}] attempt {attempts} → {role}{}", req.id, req.status, if cfg.dry_run { " (dry-run)" } else { "" });
                        st.running.insert(req.id.clone());
                        spawn_agent(cfg.clone(), state.clone(), req.clone(), role, sop);
                    }
                }
            }
        }

        // Termination: nothing pending-and-workable and nothing running.
        let active = state.lock().unwrap().running.len();
        let all_parked = {
            let st = state.lock().unwrap();
            todo.iter().all(|r| st.trackers.get(&r.id).map(|t| t.stuck).unwrap_or(false))
        };
        if active == 0 && (todo.is_empty() || all_parked) {
            if cfg.daemon {
                if idle_since.is_none() {
                    idle_since = Some(Instant::now());
                    println!("  … idle, waiting for new requests (daemon).");
                }
            } else {
                break;
            }
        } else {
            idle_since = None;
        }

        std::thread::sleep(cfg.poll);
    }

    println!("\n===== final state =====");
    for r in request::list_requests(&cfg.workspace, None)? {
        let stuck = state.lock().unwrap().trackers.get(&r.id).map(|t| t.stuck).unwrap_or(false);
        println!("  {}  [{:?}]{}  {}", r.id, r.status, if stuck { " STUCK" } else { "" }, r.title);
    }
    Ok(())
}

/// A "Workspace Team" snapshot injected into every agent's context — the team
/// roster, the pipeline, and a live view of all requests. This is what gives
/// each agent global awareness (who's upstream/downstream, current state).
fn topology(workspace: &Path, role: &str) -> String {
    let reqs = request::list_requests(workspace, None).unwrap_or_default();
    let mut t = String::from(
        "## Workspace Team\n\
         You are one agent on a pipeline team. Work flows **engineer → reviewer → qa → done**, \
         handed off automatically by request status. Agents coordinate ONLY through the shared \
         request documents and messages — never talk to each other directly.\n\
         - **engineer** — implements requests in `open` / `in_progress`\n\
         - **reviewer** — reviews requests in `review` (downstream of engineer)\n\
         - **qa** — verifies requests in `qa` (downstream of reviewer)\n\n\
         Current requests on the board:\n",
    );
    if reqs.is_empty() {
        t.push_str("  (none)\n");
    } else {
        for r in &reqs {
            t.push_str(&format!(
                "  - {} [{:?}] {}{}\n",
                r.id,
                r.status,
                r.title,
                r.claimed_by.as_deref().map(|a| format!(" (@{a})")).unwrap_or_default()
            ));
        }
    }
    t.push_str(&format!("\nYou are the **{role}** (agent name `{role}-1`).\n"));
    t
}

/// Spawn a role agent for a request on its own thread; clear `running` when done.
fn spawn_agent(cfg: Arc<Cfg>, state: Arc<Mutex<State>>, req: RequestDocument, role: &'static str, sop: &'static str) {
    std::thread::spawn(move || {
        if !cfg.dry_run {
            let topo = topology(&cfg.workspace, role);
            let prompt = format!(
                "{topo}\n{sop}\n\n---\nYou are agent `{role}-1`. The workspace tools are available \
                 as MCP tools (create_request, claim_request, get_request, list_requests, \
                 submit_engineer_work, submit_review, submit_qa, send_message, get_inbox). Follow \
                 your SOP to find the request that needs your role and complete exactly your step. \
                 The request likely waiting for you is `{}`. Start now.",
                req.id
            );
            let mut cmd = Command::new(&cfg.forge);
            cmd.arg("-p").arg(&prompt).current_dir(&cfg.project);
            if let Some(home) = &cfg.home {
                cmd.env("FORGE_CONFIG", home);
            }
            let _ = cmd.status();
        } else {
            // Simulate a session that does nothing (so stuck detection can be tested).
            std::thread::sleep(Duration::from_millis(200));
        }
        state.lock().unwrap().running.remove(&req.id);
    });
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Build an isolated base_path (used as FORGE_CONFIG) that reuses the user's real
/// provider credentials/config but exposes ONLY the workspace MCP — so spawned
/// agents don't load unrelated global MCP servers (faster startup, more reliable
/// tool registration). Credentials are symlinked, never copied.
fn setup_isolated_home(workspace: &Path, mcp_bin: &Path) -> anyhow::Result<PathBuf> {
    let home = workspace.join(".forge-home");
    std::fs::create_dir_all(&home)?;
    let real = std::env::var("FORGE_CONFIG")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            let h = home_dir();
            [".forge", "forge"].iter().map(|d| h.join(d)).find(|p| p.join(".credentials.json").exists())
        })
        .unwrap_or_else(|| home_dir().join(".forge"));
    // Reuse real provider config/credentials; exclude .mcp.json (we write our own).
    for f in [".credentials.json", ".forge.toml", ".mcp_trust.json", ".mcp-credentials.json", ".config.json"] {
        let (src, dst) = (real.join(f), home.join(f));
        if src.exists() && !dst.exists() {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&src, &dst);
            #[cfg(not(unix))]
            let _ = std::fs::copy(&src, &dst);
        }
    }
    let mcp = json!({ "mcpServers": { "forge-workspace": {
        "command": mcp_bin.to_string_lossy(),
        "env": { "FORGE_WORKSPACE_DIR": workspace.to_string_lossy() }
    }}});
    std::fs::write(home.join(".mcp.json"), serde_json::to_string_pretty(&mcp)?)?;
    Ok(home)
}

fn ensure_workspace_mcp(project: &Path, mcp_bin: &Path, workspace: &Path) -> anyhow::Result<()> {
    let path = project.join(".mcp.json");
    let mut cfg: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if cfg.get("mcpServers").is_none() {
        cfg["mcpServers"] = json!({});
    }
    cfg["mcpServers"]["forge-workspace"] = json!({
        "command": mcp_bin.to_string_lossy(),
        "env": { "FORGE_WORKSPACE_DIR": workspace.to_string_lossy() }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(())
}
