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
//! With --goal "<objective>" a coordinator (Lead) runs first and decomposes the
//! objective into requests on the board, which the pipeline then works.
//!
//! Hung agents are recovered by killing any subprocess that outlives
//! --agent-timeout-secs; a request that stays stuck past --max-attempts is
//! parked and an alert is pushed to the --alert-to inbox on the message bus.
//!
//!   forge-workspace-run --project DIR [--workspace DIR] [--forge PATH]
//!       [--goal "<objective>"] [--concurrent N] [--max-attempts N]
//!       [--poll-secs N] [--agent-timeout-secs N] [--alert-to INBOX]
//!       [--daemon] [--dry-run]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_workspace::message::{self, Category};
use forge_workspace::request::{self, RequestDocument, RequestStatus};
use serde_json::json;

const ENGINEER_SOP: &str = include_str!("../../roles/engineer.md");
const REVIEWER_SOP: &str = include_str!("../../roles/reviewer.md");
const QA_SOP: &str = include_str!("../../roles/qa.md");
const COORDINATOR_SOP: &str = include_str!("../../roles/coordinator.md");
const PM_SOP: &str = include_str!("../../roles/pm.md");
const ARCHITECT_SOP: &str = include_str!("../../roles/architect.md");

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
    /// How many times the agent working this request was killed for exceeding
    /// the timeout (a hung/runaway session).
    timeouts: u32,
    stuck: bool,
}

struct State {
    running: HashSet<String>,
    trackers: HashMap<String, Tracker>,
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
    /// Max wall-clock an agent subprocess may run before it is killed (recovery
    /// from a hung/runaway session). The freed request is retried next poll.
    agent_timeout: Duration,
    /// Inbox that stuck-request alerts are sent to (a human or lead agent).
    alert_to: String,
    /// When set (via --goal), a coordinator agent decomposes it into requests on
    /// the board at startup before the pipeline runs.
    goal: Option<String>,
    /// When set (via --plan-only), run just the coordinator and exit — decompose
    /// the goal, print the board, don't work the pipeline.
    plan_only: bool,
    /// When set (via --isolate-mcp), spawned agents run with FORGE_CONFIG=this,
    /// an isolated base_path exposing ONLY the workspace MCP.
    home: Option<PathBuf>,
    /// Optional per-role forge agent (role name → agent id), loaded from
    /// `<workspace>/.team-agents.json`. When a role has one, its `forge -p` gets
    /// `--agent <id>` — that's how a role picks a different model/persona.
    agents: HashMap<String, String>,
}

/// Load the role→agent map from `<workspace>/.team-agents.json` (if present).
fn load_role_agents(workspace: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(workspace.join(".team-agents.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .map(|m| m.into_iter().filter(|(_, v)| !v.trim().is_empty()).collect())
        .unwrap_or_default()
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
    let agents = load_role_agents(&workspace);
    let cfg = Cfg {
        project,
        workspace,
        forge,
        concurrent: flag(&mut args, "--concurrent").and_then(|s| s.parse().ok()).unwrap_or(1).max(1),
        max_attempts: flag(&mut args, "--max-attempts").and_then(|s| s.parse().ok()).unwrap_or(4),
        poll: Duration::from_secs(flag(&mut args, "--poll-secs").and_then(|s| s.parse().ok()).unwrap_or(3)),
        daemon: has(&mut args, "--daemon"),
        dry_run: has(&mut args, "--dry-run"),
        agent_timeout: Duration::from_secs(
            flag(&mut args, "--agent-timeout-secs").and_then(|s| s.parse().ok()).unwrap_or(300),
        ),
        alert_to: flag(&mut args, "--alert-to").unwrap_or_else(|| "human".into()),
        goal: flag(&mut args, "--goal").filter(|g| !g.trim().is_empty()),
        plan_only: has(&mut args, "--plan-only"),
        home,
        agents,
    };

    // With an isolated base_path the workspace MCP is provided there; otherwise
    // inject it into the project's .mcp.json (which merges with the global one).
    if cfg.home.is_none() {
        ensure_workspace_mcp(&cfg.project, &mcp_bin, &cfg.workspace)?;
    }
    println!(
        "▶ orchestrator: project={} concurrent={} max-attempts={} agent-timeout={}s alert-to={} daemon={} dry-run={} isolate-mcp={}{}",
        cfg.project.display(), cfg.concurrent, cfg.max_attempts, cfg.agent_timeout.as_secs(), cfg.alert_to,
        cfg.daemon, cfg.dry_run, cfg.home.is_some(),
        cfg.goal.as_deref().map(|g| format!(" goal={g:?}")).unwrap_or_default()
    );

    // Planning phase (⑧): PM → Architect → Lead turn the goal into a PRD,
    // design, and requests on the board before the pipeline runs. Synchronous —
    // the board must be populated first.
    if let Some(goal) = cfg.goal.clone() {
        run_planning(&cfg, &goal);
        if cfg.plan_only {
            println!("\n===== board after planning =====");
            for r in request::list_requests(&cfg.workspace, None)? {
                println!("  {}  [{:?}]  {}", r.id, r.status, r.title);
            }
            return Ok(());
        }
    }

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
                // Reset the stuck counter whenever the request advanced to a new
                // status (real progress); skip requests already parked as stuck.
                {
                    let t = st.trackers.entry(req.id.clone()).or_default();
                    if t.stuck {
                        continue;
                    }
                    if t.last_status != Some(req.status) {
                        t.last_status = Some(req.status);
                        t.attempts = 0;
                    }
                }

                // Concurrency gate BEFORE consuming an attempt: a request starved
                // by a full pool hasn't failed — it just hasn't had a turn yet, so
                // it must not accrue stuck-attempts. Only a request that actually
                // gets scheduled consumes one.
                if st.running.len() >= cfg.concurrent {
                    continue;
                }

                // This request gets a turn. Consume an attempt; park it (once) if
                // it has burned its budget without the status ever advancing.
                let attempts = {
                    let t = st.trackers.entry(req.id.clone()).or_default();
                    t.attempts += 1;
                    t.attempts
                };
                if attempts > cfg.max_attempts {
                    let timeouts = st.trackers.get(&req.id).map(|t| t.timeouts).unwrap_or(0);
                    if let Some(t) = st.trackers.get_mut(&req.id) {
                        t.stuck = true;
                    }
                    // Marking stuck makes the top-of-loop guard skip this request
                    // on every later poll, so the alert fires exactly once.
                    let body = format!(
                        "Request `{}` (\"{}\") is STUCK in {:?} after {} attempts \
                         ({} agent timeout(s)). The pipeline has parked it — a human or \
                         lead needs to intervene (clarify the request, split it, or fix a \
                         blocker), then reset it to keep it moving.",
                        req.id, req.title, req.status, cfg.max_attempts, timeouts
                    );
                    println!("  ✗ {} STUCK in {:?} after {} attempts — alerting `{}`.", req.id, req.status, cfg.max_attempts, cfg.alert_to);
                    let _ = message::send_message(&cfg.workspace, "orchestrator", &cfg.alert_to, &body, Category::Ticket);
                    continue;
                }

                let (role, sop) = role_for(req.status);
                println!("  → {} [{:?}] attempt {attempts} → {role}{}", req.id, req.status, if cfg.dry_run { " (dry-run)" } else { "" });
                st.running.insert(req.id.clone());
                spawn_agent(cfg.clone(), state.clone(), req.clone(), role, sop);
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
        let (stuck, timeouts) = {
            let st = state.lock().unwrap();
            let t = st.trackers.get(&r.id);
            (t.map(|t| t.stuck).unwrap_or(false), t.map(|t| t.timeouts).unwrap_or(0))
        };
        let flags = format!(
            "{}{}",
            if stuck { " STUCK" } else { "" },
            if timeouts > 0 { format!(" ({timeouts} timeout(s))") } else { String::new() },
        );
        println!("  {}  [{:?}]{}  {}", r.id, r.status, flags, r.title);
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
         You are one agent on a pipeline team. Planning flows **pm → architect → coordinator**, \
         then work flows **engineer → reviewer → qa → done**, handed off automatically by request \
         status. Agents coordinate ONLY through the shared request documents and messages — never \
         talk to each other directly.\n\
         - **pm** — writes the PRD (requirements + acceptance criteria) for a goal\n\
         - **architect** — designs against the PRD and decomposes it into requests\n\
         - **coordinator** (Lead) — sanity-checks the board against the goal, fills gaps\n\
         - **engineer** — implements requests in `open` / `in_progress`\n\
         - **reviewer** — reviews requests in `review` (downstream of engineer)\n\
         - **qa** — verifies requests in `qa` (downstream of reviewer)\n",
    );
    let prd = workspace.join("prd.md");
    if prd.exists() {
        t.push_str(&format!(
            "\nThe team PRD (product contract) is at `{}` — requirements and acceptance criteria \
             live there.\n",
            prd.display()
        ));
    }
    let design = workspace.join("design.md");
    if design.exists() {
        t.push_str(&format!("The architect's design notes are at `{}`.\n", design.display()));
    }
    t.push_str("\nCurrent requests on the board:\n");
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

/// The planning chain: PM writes the PRD, the Architect designs and creates the
/// requests, the Lead sanity-checks the board. Each step runs synchronously so
/// the next one sees its artifacts; the pipeline starts on a populated board.
fn run_planning(cfg: &Cfg, goal: &str) {
    let before = request::list_requests(&cfg.workspace, None).map(|r| r.len()).unwrap_or(0);
    let prd = cfg.workspace.join("prd.md");
    let design = cfg.workspace.join("design.md");

    run_planner(
        cfg,
        "pm",
        PM_SOP,
        &format!(
            "Your objective:\n\n{goal}\n\nWrite the PRD to `{}` (create or overwrite that exact \
             file). Follow your SOP.",
            prd.display()
        ),
    );

    let prd_note = if prd.exists() {
        format!("The PM's PRD is at `{}` — read it FIRST.", prd.display())
    } else {
        "No PRD file was produced; design directly from the objective.".to_string()
    };
    run_planner(
        cfg,
        "architect",
        ARCHITECT_SOP,
        &format!(
            "The objective:\n\n{goal}\n\n{prd_note} Write your design notes to `{}`, then \
             decompose into work requests via create_request. Follow your SOP.",
            design.display()
        ),
    );

    run_planner(
        cfg,
        "coordinator",
        COORDINATOR_SOP,
        &format!(
            "The objective:\n\n{goal}\n\nThe PM and architect have already planned: the PRD is at \
             `{}` and the architect created the requests now on the board. Your job is a final \
             sanity pass: compare the board against the objective/PRD, create requests ONLY for \
             genuine gaps (never duplicates), and report the final plan (request ids + titles).",
            prd.display()
        ),
    );

    let after = request::list_requests(&cfg.workspace, None).map(|r| r.len()).unwrap_or(before);
    println!("  ⚑ planning done — {} request(s) created.", after.saturating_sub(before));
}

/// Run one synchronous planning-phase agent (pm / architect / coordinator).
fn run_planner(cfg: &Cfg, role: &str, sop: &str, tail: &str) {
    println!("  ⚑ {role} planning{}", if cfg.dry_run { " (dry-run)" } else { "" });
    if cfg.dry_run {
        return;
    }
    let topo = topology(&cfg.workspace, role);
    let prompt = format!(
        "{topo}\n{sop}\n\n---\nYou are agent `{role}-1`. The workspace tools are available as MCP \
         tools (create_request, list_requests, get_request, send_message).\n\n{tail}\n\nStart now."
    );
    let mut cmd = Command::new(&cfg.forge);
    cmd.arg("-p").arg(&prompt).current_dir(&cfg.project);
    if let Some(agent) = cfg.agents.get(role) {
        cmd.arg("--agent").arg(agent);
    }
    if let Some(home) = &cfg.home {
        cmd.env("FORGE_CONFIG", home);
    }
    let _ = cmd.status();
}

/// Spawn a role agent for a request on its own thread; clear `running` when done.
///
/// The subprocess is bounded by `cfg.agent_timeout`: if it runs longer it is
/// killed (recovery from a hung/runaway agent) so the concurrency slot is freed
/// and the request is retried on the next poll instead of wedging the pipeline.
fn spawn_agent(cfg: Arc<Cfg>, state: Arc<Mutex<State>>, req: RequestDocument, role: &'static str, sop: &'static str) {
    std::thread::spawn(move || {
        let mut timed_out = false;
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
            if let Some(agent) = cfg.agents.get(role) {
                cmd.arg("--agent").arg(agent);
            }
            if let Some(home) = &cfg.home {
                cmd.env("FORGE_CONFIG", home);
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    let deadline = Instant::now() + cfg.agent_timeout;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break, // exited on its own
                            Ok(None) => {
                                if Instant::now() >= deadline {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    timed_out = true;
                                    println!(
                                        "  ⏱ {} [{:?}] agent exceeded {}s — killed; will retry.",
                                        req.id, req.status, cfg.agent_timeout.as_secs()
                                    );
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(500));
                            }
                            Err(e) => {
                                eprintln!("  ! {} wait error: {e}", req.id);
                                break;
                            }
                        }
                    }
                }
                Err(e) => eprintln!("  ! {} failed to spawn forge: {e}", req.id),
            }
        } else {
            // Simulate a session that does nothing (so stuck detection can be tested).
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut guard = state.lock().unwrap();
        if timed_out {
            if let Some(t) = guard.trackers.get_mut(&req.id) {
                t.timeouts += 1;
            }
        }
        guard.running.remove(&req.id);
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
