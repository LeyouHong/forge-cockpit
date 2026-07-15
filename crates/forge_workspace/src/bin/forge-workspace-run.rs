//! The orchestrator — the resident "team brain".
//!
//! An event-driven loop over the workspace: every member is a resident tmux
//! terminal running an interactive CLI agent (Claude Code on the user's
//! subscription — no provider API key). For each pending request the
//! orchestrator pastes the role prompt (SOP + topology) into the member's
//! pane; agents drive status forward through the workspace MCP tools and the
//! orchestrator re-reads the board and reacts. Compared to a one-shot runner
//! it adds:
//!
//!   - concurrency     — several requests worked in parallel (--concurrent N)
//!   - stuck detection — if a request sits in one status for too many attempts,
//!                       it is marked stuck and left alone (no infinite token burn)
//!   - daemon mode     — keep running and pick up new requests (--daemon)
//!
//! With --goal "<objective>" a coordinator (Lead) runs first and decomposes the
//! objective into requests on the board, which the pipeline then works.
//!
//! A member that makes no board progress within --agent-timeout-secs frees
//! its slot (the pane itself is never killed — a human may be attached); a
//! request that stays stuck past --max-attempts is parked and an alert is
//! pushed to the --alert-to inbox on the message bus.
//!
//!   forge-workspace-run --project DIR [--workspace DIR]
//!       [--goal "<objective>"] [--concurrent N] [--max-attempts N]
//!       [--poll-secs N] [--agent-timeout-secs N] [--alert-to INBOX]
//!       [--daemon] [--dry-run]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_workspace::memory;
use forge_workspace::message::{self, Category};
use forge_workspace::request::{self, RequestDocument, RequestStatus};
use forge_workspace::team::{self, Stage, TeamConfig, TeamMember};
use forge_workspace::terminal;
use forge_workspace::watch::{self, WatchAction};
use serde_json::json;

const ENGINEER_SOP: &str = include_str!("../../roles/engineer.md");
const REVIEWER_SOP: &str = include_str!("../../roles/reviewer.md");
const QA_SOP: &str = include_str!("../../roles/qa.md");
const COORDINATOR_SOP: &str = include_str!("../../roles/coordinator.md");
const PM_SOP: &str = include_str!("../../roles/pm.md");
const ARCHITECT_SOP: &str = include_str!("../../roles/architect.md");
const VISUAL_VERIFY: &str = include_str!("../../roles/visual-verify.md");

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
    /// Terminal-resident members currently working a request. A resident
    /// terminal is one pane — it can only take one prompt at a time, so other
    /// requests for the same member wait (without burning stuck-attempts).
    busy_members: HashSet<String>,
}

struct Cfg {
    project: PathBuf,
    workspace: PathBuf,
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
    /// The team roster (`<workspace>/.team.json`, or the built-in six roles).
    team: TeamConfig,
}

fn pending(r: &RequestDocument) -> bool {
    matches!(
        r.status,
        RequestStatus::Open | RequestStatus::InProgress | RequestStatus::Review | RequestStatus::Qa
    )
}

/// The built-in SOP for a well-known member id.
fn builtin_sop(id: &str) -> Option<&'static str> {
    match id {
        "pm" => Some(PM_SOP),
        "architect" => Some(ARCHITECT_SOP),
        "coordinator" => Some(COORDINATOR_SOP),
        "engineer" => Some(ENGINEER_SOP),
        "reviewer" => Some(REVIEWER_SOP),
        "qa" => Some(QA_SOP),
        _ => None,
    }
}

/// A member's SOP: its custom role_prompt, else the built-in SOP for its id,
/// else the built-in SOP of its stage (so a custom second engineer works).
fn member_sop(m: &TeamMember) -> String {
    if !m.role_prompt.trim().is_empty() {
        return m.role_prompt.clone();
    }
    if let Some(s) = builtin_sop(&m.id) {
        return s.to_string();
    }
    match m.stage {
        Stage::Plan => COORDINATOR_SOP.to_string(),
        Stage::Implement => ENGINEER_SOP.to_string(),
        Stage::Review => REVIEWER_SOP.to_string(),
        Stage::Qa => QA_SOP.to_string(),
    }
}

fn stage_for(status: RequestStatus) -> Stage {
    match status {
        RequestStatus::Open | RequestStatus::InProgress => Stage::Implement,
        RequestStatus::Review => Stage::Review,
        RequestStatus::Qa => Stage::Qa,
        RequestStatus::Done | RequestStatus::Rejected => {
            unreachable!("stage_for called with terminal status {:?} — pending() filters these", status)
        }
    }
}

/// The built-in SOP for the work a stage does (used for gap coverage).
fn stage_sop(stage: Stage) -> &'static str {
    match stage {
        Stage::Plan => COORDINATOR_SOP,
        Stage::Implement => ENGINEER_SOP,
        Stage::Review => REVIEWER_SOP,
        Stage::Qa => QA_SOP,
    }
}

/// Verify-stage members (review/qa) get the graceful visual-verification
/// protocol appended, so UI work can be checked visually when a browser is
/// present and degrades to code review when it isn't.
fn with_visual_verify(sop: String, stage: Stage) -> String {
    if matches!(stage, Stage::Review | Stage::Qa) {
        format!("{sop}\n\n{VISUAL_VERIFY}")
    } else {
        sop
    }
}

/// The member that works a request in `status` — stable pick (by request id)
/// when several members share the stage, so retries reuse the same member.
///
/// When NO member handles the stage, the Lead covers the gap (mirrors the
/// reference forge's Lead SOP): coordinator if present, else any plan-stage
/// member, else the first member. The second tuple field is true in that case
/// so the spawn swaps in the stage's SOP instead of the member's own.
enum Pick {
    /// A member takes it (bool = Lead covering a stage gap).
    Member(TeamMember, bool),
    /// Everyone who could take it is paused — the request waits. Pause means
    /// "hold this stage's work", so held requests are neither rerouted to
    /// other stages nor covered by the Lead.
    Held,
    /// The team is empty.
    Empty,
}

fn member_for(team: &TeamConfig, paused: &HashSet<String>, status: RequestStatus, req_id: &str) -> Pick {
    let stage = stage_for(status);
    let pool: Vec<&TeamMember> = team.members.iter().filter(|m| m.stage == stage).collect();
    if !pool.is_empty() {
        let avail: Vec<&&TeamMember> = pool.iter().filter(|m| !paused.contains(&m.id)).collect();
        if avail.is_empty() {
            return Pick::Held;
        }
        let n: usize = req_id.bytes().map(|b| b as usize).sum();
        return Pick::Member((*avail[n % avail.len()]).clone(), false);
    }
    let Some(lead) = team
        .members
        .iter()
        .find(|m| m.id == "coordinator")
        .or_else(|| team.members.iter().find(|m| m.stage == Stage::Plan))
        .or_else(|| team.members.first())
    else {
        return Pick::Empty;
    };
    if paused.contains(&lead.id) {
        return Pick::Held;
    }
    Pick::Member(lead.clone(), true)
}

/// Approval gate state, persisted at `<workspace>/.team-approvals.json` as
/// `{"<req_id>@<Status>": "pending" | "approved"}`. Approving is done by the
/// web UI (or by editing the file); the entry is consumed on spawn so each
/// new status pass needs a fresh approval.
fn approval_key(req: &RequestDocument) -> String {
    format!("{}@{:?}", req.id, req.status)
}

fn approvals_path(workspace: &Path) -> PathBuf {
    workspace.join(".team-approvals.json")
}

fn load_approvals(workspace: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(approvals_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_approvals(workspace: &Path, map: &HashMap<String, String>) {
    let _ = std::fs::write(approvals_path(workspace), serde_json::to_string_pretty(map).unwrap_or_default());
}

/// Returns true when the member may work this request now. Creates the
/// pending entry (and alerts the human once) on first sight.
fn approval_granted(cfg: &Cfg, req: &RequestDocument, member: &TeamMember) -> bool {
    if !member.requires_approval {
        return true;
    }
    let key = approval_key(req);
    let mut map = load_approvals(&cfg.workspace);
    match map.get(&key).map(String::as_str) {
        Some("approved") => {
            map.remove(&key); // consumed — the next status pass re-asks
            save_approvals(&cfg.workspace, &map);
            true
        }
        Some(_) => false, // pending — wait for the human
        None => {
            map.insert(key.clone(), "pending".into());
            save_approvals(&cfg.workspace, &map);
            let body = format!(
                "APPROVAL NEEDED: member `{}` requires approval to work request `{}` (\"{}\") in \
                 [{:?}]. Approve it on the Team page (or mark `{key}` approved in .team-approvals.json).",
                member.id, req.id, req.title, req.status
            );
            let _ = message::send_message(&cfg.workspace, "orchestrator", &cfg.alert_to, &body, Category::Ticket);
            println!("  ⏸ {} [{:?}] waiting for approval of member `{}`", req.id, req.status, member.id);
            false
        }
    }
}

/// Per-member session ids give each agent a PERSISTENT conversation that
/// survives across tasks: the id doubles as the Claude Code session id
/// (`--session-id` on first launch, `--resume` after), so a member keeps its
/// memory (what it built, decided, was told) instead of starting cold.
/// Stored at `<workspace>/.team-sessions.json` (member id -> session id).
fn sessions_path(workspace: &Path) -> PathBuf {
    workspace.join(".team-sessions.json")
}

fn get_or_create_session(workspace: &Path, member_id: &str) -> String {
    let path = sessions_path(workspace);
    let mut map: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let Some(id) = map.get(member_id) {
        return id.clone();
    }
    // Reuse forge's id shape so `--conversation-id` accepts it.
    let id = forge_workspace::pipeline::new_conversation_id();
    map.insert(member_id.to_string(), id.clone());
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&map).unwrap_or_default());
    id
}

/// Per-member live log (the resident session's streamed output).
fn member_log_path(workspace: &Path, member_id: &str) -> PathBuf {
    workspace.join(".team-logs").join(format!("{member_id}.log"))
}

/// Open (append) the member log, prefixed with a run banner.
fn open_member_log(workspace: &Path, member_id: &str, banner: &str) -> Option<std::fs::File> {
    let path = member_log_path(workspace, member_id);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    use std::io::Write;
    let _ = writeln!(f, "\n===== {} {} =====", now_banner(), banner);
    Some(f)
}

fn now_banner() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Write `<workspace>/.team-status.json`: per-member `{status, request}` plus
/// whether each has a persistent session log. Read by the Team page.
fn write_member_status(cfg: &Cfg, state: &Arc<Mutex<State>>, todo: &[RequestDocument]) {
    let running = state.lock().unwrap().running.clone();
    let paused = team::load_paused(&cfg.workspace);
    // Which member owns each running request (stable pick, mirrors scheduling
    // — including the paused filter, so attribution matches who was actually
    // picked; a member paused mid-run simply shows paused once it finishes).
    let mut working: HashMap<String, String> = HashMap::new();
    for req in todo {
        if running.contains(&req.id) {
            if let Pick::Member(m, _) = member_for(&cfg.team, &paused, req.status, &req.id) {
                working.insert(m.id, req.id.clone());
            }
        }
    }
    write_status_file(cfg, &working, &paused);
}

/// A plan-stage member works before the board exists, so it can never appear in
/// the request-derived `working` map — mark it working explicitly, otherwise
/// planners look idle for the minutes they are actually the only ones running.
fn write_planning_status(cfg: &Cfg, planner: Option<&str>) {
    let paused = team::load_paused(&cfg.workspace);
    let mut working: HashMap<String, String> = HashMap::new();
    if let Some(id) = planner {
        working.insert(id.to_string(), "planning".to_string());
    }
    write_status_file(cfg, &working, &paused);
}

/// `<workspace>/.team-status.json` — per-member `{status, request}` plus the
/// live tmux session and pause flag. Read by the Team page.
fn write_status_file(cfg: &Cfg, working: &HashMap<String, String>, paused: &HashSet<String>) {
    let mut out = serde_json::Map::new();
    for m in &cfg.team.members {
        let has_log = member_log_path(&cfg.workspace, &m.id).exists();
        let (status, request) = match working.get(&m.id) {
            Some(r) => ("working", Some(r.clone())),
            None if paused.contains(&m.id) => ("paused", None),
            None => ("idle", None),
        };
        // Expose the live tmux session (when it exists) so the UI can offer
        // the in-cockpit terminal / `tmux attach`.
        let session = terminal::session_name(&cfg.project, &m.id);
        let tmux = terminal::has_session(&session).then_some(session);
        out.insert(
            m.id.clone(),
            serde_json::json!({
                "status": status, "request": request, "has_log": has_log,
                "terminal": tmux, "paused": paused.contains(&m.id),
            }),
        );
    }
    let path = cfg.workspace.join(".team-status.json");
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap_or_default());
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

    // Explicit teardown: kill this project's resident terminals and exit,
    // without running the pipeline (the CLI counterpart to the web Stop button
    // and to aiwatching/forge's `--reset-terminal`).
    if has(&mut args, "--reset-terminals") {
        let killed = terminal::kill_project_sessions(&project);
        if killed.is_empty() {
            println!("no resident terminals for {} — nothing to tear down.", project.display());
        } else {
            for name in &killed {
                println!("  ⏹ tore down resident terminal `{name}`");
            }
        }
        return Ok(());
    }

    let workspace = flag(&mut args, "--workspace").map(PathBuf::from).unwrap_or_else(|| project.join(".forge-workspace"));
    std::fs::create_dir_all(&workspace)?;
    let workspace = workspace.canonicalize()?;
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
    let mcp_bin = exe_dir.join("forge-workspace-mcp");
    // Accepted and ignored for backward compatibility (pre-terminal-only callers).
    let _ = flag(&mut args, "--forge");
    let _ = has(&mut args, "--isolate-mcp");
    let team_cfg = team::load_team(&workspace);
    if let Err(e) = team::validate_team(&team_cfg) {
        anyhow::bail!("invalid team config (<workspace>/.team.json): {e}");
    }
    let cfg = Cfg {
        project,
        workspace,
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
        team: team_cfg,
    };

    // Members ARE terminals — tmux and the .mcp.json wiring are hard
    // requirements, checked up front so failure is one clear message.
    if !cfg.dry_run && !terminal::tmux_available() {
        anyhow::bail!("team members run as resident terminals, but tmux is not installed / not on PATH");
    }
    ensure_workspace_mcp(&cfg.project, &mcp_bin, &cfg.workspace)?;
    if let Err(e) = ensure_stop_hook(&cfg.project) {
        eprintln!("  ! could not install Stop hook (agents fall back to timeout polling): {e:#}");
    }
    println!(
        "▶ orchestrator: project={} concurrent={} max-attempts={} agent-timeout={}s alert-to={} daemon={} dry-run={}{}",
        cfg.project.display(), cfg.concurrent, cfg.max_attempts, cfg.agent_timeout.as_secs(), cfg.alert_to,
        cfg.daemon, cfg.dry_run,
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

    let state = Arc::new(Mutex::new(State {
        running: HashSet::new(),
        trackers: HashMap::new(),
        busy_members: HashSet::new(),
    }));
    let cfg = Arc::new(cfg);
    let mut idle_since: Option<Instant> = None;
    // Resident-session hygiene: sweep for `forge-team-*` terminals left idle for
    // hours (finished teams, other projects, older runs) so the tmux server
    // doesn't accumulate dead sessions. Runs on the first poll and every 5 min
    // after; a busy session keeps recent activity and is never reaped.
    let reap_every = Duration::from_secs(5 * 60);
    let mut last_reap: Option<Instant> = None;

    loop {
        if !cfg.dry_run && last_reap.map_or(true, |t| t.elapsed() >= reap_every) {
            last_reap = Some(Instant::now());
            for name in terminal::reap_idle_sessions(terminal::SESSION_IDLE) {
                println!("  ♻ reaped idle terminal `{name}` (unused > {}h)", terminal::SESSION_IDLE.as_secs() / 3600);
            }
        }

        // Bus upkeep, before any scheduling:
        //   1. liveness — a member is up iff its pane is; coming back flushes
        //      the outbox, so mail sent while it was down lands now.
        //   2. retries  — tickets/requests not acked within the ACK window
        //      resurface in the recipient's inbox; exhausted ones fail and
        //      alert a human (a dropped rework request is silent otherwise).
        for m in &cfg.team.members {
            let live = if terminal::has_session(&terminal::session_name(&cfg.project, &m.id)) {
                message::Liveness::Alive
            } else {
                message::Liveness::Down
            };
            let _ = message::set_liveness(&cfg.workspace, &format!("{}-1", m.id), live);
        }
        let (retried, failed) = message::retry_stale(&cfg.workspace);
        if !retried.is_empty() {
            println!("  ↻ bus: {} message(s) re-delivered (no ack within {}s)", retried.len(), message::ACK_TIMEOUT_SECS);
        }
        for id in &failed {
            let body = format!(
                "Message `{id}` was never acknowledged after {} delivery attempts — the recipient \
                 is ignoring it or cannot act. A human should look.",
                message::MAX_RETRIES
            );
            println!("  ✗ bus: message {id} FAILED after {} attempts — alerting `{}`.", message::MAX_RETRIES, cfg.alert_to);
            let _ = message::send_message(&cfg.workspace, "bus", &cfg.alert_to, &body, Category::Ticket);
        }

        // Watches: evaluate due monitors and route what fired. A `request`
        // watch lands on the board and is scheduled below like any other
        // work; an `alert` watch tickets the human inbox. In daemon mode
        // this is what wakes an idle team when the world changes.
        for f in watch::tick(&cfg.project, &cfg.workspace) {
            match f.action {
                WatchAction::Request if !cfg.dry_run => {
                    match request::create_request(
                        &cfg.workspace,
                        request::NewRequest {
                            title: format!("[watch:{}] {}", f.watch_id, f.headline),
                            description: f.body,
                            acceptance_criteria: Vec::new(),
                            batch: None,
                        },
                    ) {
                        Ok(r) => println!("  ⚑ watch `{}` fired → {}", f.watch_id, r.id),
                        Err(e) => eprintln!("  ! watch `{}` fired but create_request failed: {e:#}", f.watch_id),
                    }
                }
                WatchAction::Request => println!("  ⚑ watch `{}` fired (dry-run, no request)", f.watch_id),
                WatchAction::Alert => {
                    let _ = message::send_message(
                        &cfg.workspace,
                        "watch-manager",
                        &cfg.alert_to,
                        &format!("Watch `{}`: {}\n\n{}", f.watch_id, f.headline, f.body),
                        Category::Ticket,
                    );
                    println!("  ⚑ watch `{}` fired → alerted `{}`", f.watch_id, cfg.alert_to);
                }
            }
        }

        let reqs = request::list_requests(&cfg.workspace, None)?;
        let todo: Vec<RequestDocument> = reqs.into_iter().filter(pending).collect();

        // Pause flags are re-read every poll so a ⏸ pressed in the web UI
        // takes effect at the next scheduling decision without a restart.
        let paused = team::load_paused(&cfg.workspace);

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

                // Approval gate — checked before the attempt counter so a
                // request waiting on a human never accrues stuck-attempts.
                // (Held-by-pause likewise consumes nothing: waiting for a
                // human to press ▶ is not failure.)
                let (member, covering) = match member_for(&cfg.team, &paused, req.status, &req.id) {
                    Pick::Member(m, c) => (m, c),
                    Pick::Held => continue,
                    Pick::Empty => {
                        println!("  ! {} [{:?}] team is empty — skipping", req.id, req.status);
                        continue;
                    }
                };
                if !cfg.dry_run && !approval_granted(&cfg, req, &member) {
                    continue;
                }

                // One prompt at a time per resident terminal: like the
                // concurrency gate above, waiting for the member's pane is not
                // failure, so no attempt is consumed.
                if st.busy_members.contains(&member.id) {
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

                println!(
                    "  → {} [{:?}] attempt {attempts} → {}{}{}",
                    req.id,
                    req.status,
                    member.id,
                    if covering { " (covering gap — no member for this stage)" } else { "" },
                    if cfg.dry_run { " (dry-run)" } else { "" }
                );
                st.running.insert(req.id.clone());
                st.busy_members.insert(member.id.clone());
                spawn_agent(cfg.clone(), state.clone(), req.clone(), member, covering);
            }
        }

        // Publish per-member status (down/idle/working) for the web UI: a member
        // is `working` when it owns a running request, else `idle`. `down` is the
        // absence of the orchestrator pid, decided by the reader.
        write_member_status(&cfg, &state, &todo);

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
fn topology(cfg: &Cfg, role: &str) -> String {
    let workspace = &cfg.workspace;
    let reqs = request::list_requests(workspace, None).unwrap_or_default();
    let mut t = String::from(
        "## Workspace Team\n\
         You are one agent on a pipeline team. Plan-stage members run first (in dependency \
         order), then work flows through request statuses: implement-stage members take \
         `open`/`in_progress`, review-stage `review`, qa-stage `qa`. Agents coordinate ONLY \
         through the shared request documents and messages — never talk to each other directly.\n\
         Team roster:\n",
    );
    for m in &cfg.team.members {
        let deps = if m.depends_on.is_empty() {
            String::new()
        } else {
            format!(" — downstream of {}", m.depends_on.join(", "))
        };
        t.push_str(&format!(
            "  - **{}** ({}, {:?} stage){}\n",
            m.id,
            m.name,
            m.stage,
            deps
        ));
    }
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
    // Prepend this member's own persistent memory recap (progressive disclosure:
    // recent observations in full, older title-only), so an agent starting a new
    // turn — or a fresh session — sees what it learned before.
    let recap = memory::format_for_prompt(&memory::load(workspace, role));
    if !recap.is_empty() {
        t.push('\n');
        t.push_str(&recap);
    }
    t
}

/// The planning chain: every plan-stage member runs once, synchronously, in
/// dependency order (PM → Architect → Lead in the default team). Each sees the
/// artifacts its upstream wrote; the pipeline starts on a populated board.
fn run_planning(cfg: &Cfg, goal: &str) {
    let before = request::list_requests(&cfg.workspace, None).map(|r| r.len()).unwrap_or(0);
    let prd = cfg.workspace.join("prd.md");
    let design = cfg.workspace.join("design.md");

    let ordered = team::topo_order(&cfg.team.members).unwrap_or_else(|_| cfg.team.members.clone());
    for m in ordered.iter().filter(|m| m.stage == Stage::Plan) {
        let upstream_note = if m.depends_on.is_empty() {
            "You are the FIRST planner — nothing exists yet.".to_string()
        } else {
            format!(
                "Members upstream of you ({}) have already planned. Read their artifacts and the \
                 board first, and do NOT duplicate their work — only add what your SOP owns.",
                m.depends_on.join(", ")
            )
        };
        let tail = format!(
            "The team objective:\n\n{goal}\n\nShared planning artifacts live in the workspace: \
             the PRD belongs at `{}` and design notes at `{}`. {} If your SOP says to create work \
             requests, use create_request.",
            prd.display(),
            design.display(),
            upstream_note
        );
        run_planner(cfg, m, &tail);
    }

    let after = request::list_requests(&cfg.workspace, None).map(|r| r.len()).unwrap_or(before);
    println!("  ⚑ planning done — {} request(s) created.", after.saturating_sub(before));
}

/// Run one synchronous planning-phase member.
fn run_planner(cfg: &Cfg, m: &TeamMember, tail: &str) {
    println!("  ⚑ {} planning{}", m.id, if cfg.dry_run { " (dry-run)" } else { "" });
    if cfg.dry_run {
        return;
    }
    let topo = topology(cfg, &m.id);
    let sop = member_sop(m);
    let prompt = format!(
        "{topo}\n{sop}\n\n---\nYou are agent `{id}-1`. The workspace tools are available as MCP \
         tools (create_request, list_requests, get_request, send_message, record_observation). \
         Use record_observation to save key planning decisions for later turns.\n\n{tail}\n\nStart now.",
        id = m.id
    );
    write_planning_status(cfg, Some(&m.id));
    run_terminal_planner(cfg, m, &prompt);
    write_planning_status(cfg, None);
}

/// Planning through a resident terminal. Unlike work requests there is no
/// status transition to watch — a planner's output is artifacts (PRD, design,
/// board entries) — so completion is signalled over the message bus: the
/// prompt asks the agent to message `orchestrator` with PLANNING-DONE, and we
/// wait for that (bounded by the agent timeout).
fn run_terminal_planner(cfg: &Cfg, m: &TeamMember, prompt: &str) {
    let name = terminal::session_name(&cfg.project, &m.id);
    let prompt = format!(
        "{prompt}\n\nIMPORTANT: when your planning work is completely finished, call send_message \
         with to=`orchestrator` and a body starting with `PLANNING-DONE` — the pipeline waits for \
         that signal before the next planner runs."
    );
    if let Err(e) = ensure_member_terminal(cfg, m, &name) {
        eprintln!("  ! planner `{}` terminal: {e}", m.id);
        return;
    }
    // Baseline the Stop-hook turn marker BEFORE injecting: a marker newer than
    // this means the agent finished its turn after we prompted it.
    let turn0 = terminal::turn_marker(&cfg.workspace, &m.id);
    if let Err(e) = terminal::send_text(&name, &cfg.workspace, &prompt) {
        eprintln!("  ! planner `{}` prompt injection failed: {e}", m.id);
        return;
    }
    let deadline = Instant::now() + cfg.agent_timeout;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let done_msg = message::get_inbox(&cfg.workspace, "orchestrator", true)
            .unwrap_or_default()
            .into_iter()
            .find(|msg| msg.from.starts_with(&m.id) && msg.body.contains("PLANNING-DONE"));
        if let Some(msg) = done_msg {
            let _ = message::ack(&cfg.workspace, "orchestrator", &msg.id);
            break;
        }
        // Event-driven fail-fast: the agent's turn ended (Stop hook fired) but no
        // PLANNING-DONE — it's done responding and won't produce more, so move on
        // now instead of waiting out the timeout. (No hook installed → marker
        // never advances → we fall back to the deadline below, as before.)
        if terminal::turn_marker(&cfg.workspace, &m.id) > turn0 {
            println!("  ⤷ planner `{}` finished its turn without PLANNING-DONE — moving on.", m.id);
            break;
        }
        if Instant::now() >= deadline {
            println!(
                "  ⏱ planner `{}` sent no PLANNING-DONE within {}s — moving on.",
                m.id,
                cfg.agent_timeout.as_secs()
            );
            break;
        }
    }
    if let Some(log) = open_member_log(&cfg.workspace, &m.id, "terminal planning") {
        use std::io::Write;
        let mut log = log;
        let _ = writeln!(log, "{}", terminal::capture_pane(&name));
    }
}

/// Spawn a role agent for a request on its own thread; clear `running` when done.
///
/// The subprocess is bounded by `cfg.agent_timeout`: if it runs longer it is
/// killed (recovery from a hung/runaway agent) so the concurrency slot is freed
/// and the request is retried on the next poll instead of wedging the pipeline.
fn spawn_agent(cfg: Arc<Cfg>, state: Arc<Mutex<State>>, req: RequestDocument, member: TeamMember, covering: bool) {
    std::thread::spawn(move || {
        let mut timed_out = false;
        let mut fatal = false;
        let mut interrupted = false;
        if !cfg.dry_run {
            let topo = topology(&cfg, &member.id);
            // Gap coverage: the team has no member for this request's stage, so
            // the Lead does the stage's work itself — stage SOP, not its own.
            let sop = if covering {
                format!(
                    "## Gap coverage\nYour team has NO dedicated member for this request's current \
                     stage — as the Lead you are covering it yourself for this request. Do the \
                     stage's work per the SOP below and submit through the normal tools; do not \
                     wait for or delegate to a member that doesn't exist.\n\n{}",
                    stage_sop(stage_for(req.status))
                )
            } else {
                member_sop(&member)
            };
            let sop = with_visual_verify(sop, if covering { stage_for(req.status) } else { member.stage });
            let prompt = format!(
                "{topo}\n{sop}\n\n---\nYou are agent `{role}-1`. The workspace tools are available \
                 as MCP tools (create_request, claim_request, get_request, list_requests, \
                 submit_engineer_work, submit_review, submit_qa, send_message, get_inbox, \
                 ack_message, ask_agent, reply_to_agent, record_observation). Save anything \
                 worth remembering across turns (a decision, a fix, a discovery) with \
                 record_observation — your recent notes are shown under \"Your memory\" above. \
                 Check get_inbox before you start: a \
                 ticket there is retried until you ack_message it, and a request must be answered \
                 with reply_to_agent. When you are blocked on a teammate's decision, ask_agent \
                 waits for their answer instead of guessing. Follow \
                 your SOP to find the request that needs your role and complete exactly your step. \
                 The request likely waiting for you is `{}`. Start now.",
                req.id,
                role = member.id
            );
            (timed_out, fatal, interrupted) = match run_terminal_request(&cfg, &req, &member, &prompt) {
                TurnOutcome::Advanced => (false, false, false),
                TurnOutcome::TimedOut => (true, false, false),
                TurnOutcome::Interrupted => (false, false, true),
                TurnOutcome::Unreachable => (false, true, false),
            };
        } else {
            // Simulate a session that does nothing (so stuck detection can be tested).
            std::thread::sleep(Duration::from_millis(200));
        }
        finish_agent(&state, &req.id, &member, timed_out, cfg.max_attempts, fatal, interrupted);
    });
}

/// Release the request's slot (and the member's pane, for terminal members).
///
/// When `fatal` is true the terminal is unreachable and won't self-heal — the
/// attempt counter is bumped directly to `max_attempts` so the human is alerted
/// immediately instead of waiting through retry cycles.
fn finish_agent(state: &Arc<Mutex<State>>, req_id: &str, member: &TeamMember, timed_out: bool, max_attempts: u32, fatal: bool, interrupted: bool) {
    let mut guard = state.lock().unwrap();
    if fatal {
        if let Some(t) = guard.trackers.get_mut(req_id) {
            t.attempts = max_attempts;
        }
    } else if timed_out {
        if let Some(t) = guard.trackers.get_mut(req_id) {
            t.timeouts += 1;
        }
    } else if interrupted {
        // A human interrupted this turn — refund the attempt it consumed so a
        // deliberate stop doesn't push the request toward the stuck threshold.
        if let Some(t) = guard.trackers.get_mut(req_id) {
            t.attempts = t.attempts.saturating_sub(1);
        }
    }
    guard.running.remove(req_id);
    guard.busy_members.remove(&member.id);
}

/// How one terminal turn ended, so the caller can account for it correctly.
enum TurnOutcome {
    /// The request left the member's stage — real progress.
    Advanced,
    /// No progress before the agent timeout — free the slot and retry.
    TimedOut,
    /// A human interrupted the current turn — stop it, but don't penalize the
    /// request (the attempt is refunded so it retries fresh, not toward stuck).
    Interrupted,
    /// The terminal is unreachable and won't self-heal — alert a human.
    Unreachable,
}

/// Drive one request through a terminal-resident member: make sure its tmux
/// pane is alive, paste the prompt, then watch the board until the request
/// leaves the member's stage (the agent submits through the workspace MCP), the
/// timeout passes, or a human interrupts. The pane itself is never killed — it
/// is the member's resident session, and a human may be attached.
fn run_terminal_request(cfg: &Cfg, req: &RequestDocument, member: &TeamMember, prompt: &str) -> TurnOutcome {
    let name = terminal::session_name(&cfg.project, &member.id);
    if let Err(e) = ensure_member_terminal(cfg, member, &name) {
        eprintln!("  ! {} terminal `{name}`: {e}", req.id);
        // Terminal unreachable — this won't self-heal; the caller will
        // fast-track the request to stuck so the human is alerted immediately.
        return TurnOutcome::Unreachable;
    }
    // Baseline the Stop-hook turn marker before injecting (see run_terminal_planner).
    let turn0 = terminal::turn_marker(&cfg.workspace, &member.id);
    if let Err(e) = terminal::send_text(&name, &cfg.workspace, prompt) {
        eprintln!("  ! {} terminal `{name}` prompt injection failed: {e}", req.id);
        return TurnOutcome::TimedOut;
    }
    let start_stage = stage_for(req.status);
    let deadline = Instant::now() + cfg.agent_timeout;
    let outcome = loop {
        std::thread::sleep(Duration::from_secs(2));
        // A human pressed ⎋ Interrupt on this member: halt the current turn (the
        // resident session survives) and yield the slot so it can retry fresh.
        if team::take_interrupt(&cfg.workspace, &member.id) {
            println!("  ⎋ {} [{:?}] member `{}` interrupted — stopping the current turn.", req.id, req.status, member.id);
            terminal::interrupt(&name);
            break TurnOutcome::Interrupted;
        }
        // Advanced = the request left this member's stage (done/rejected count).
        // A same-stage transition (open → in_progress via claim) is the member
        // still working, not a handoff — keep waiting.
        let advanced = match request::get_request(&cfg.workspace, &req.id) {
            Ok(Some((cur, _))) => !pending(&cur) || stage_for(cur.status) != start_stage,
            Ok(None) => true, // request removed — nothing left to wait for
            Err(_) => false,
        };
        if advanced {
            break TurnOutcome::Advanced;
        }
        // Event-driven fail-fast: the agent's turn ended (Stop hook fired) but the
        // request didn't advance — this attempt is done; free the slot and retry
        // now rather than waiting out the timeout. (No hook → falls back to deadline.)
        if terminal::turn_marker(&cfg.workspace, &member.id) > turn0 {
            println!(
                "  ⤷ {} [{:?}] member `{}` finished its turn without advancing — freeing the slot; will retry.",
                req.id, req.status, member.id
            );
            break TurnOutcome::TimedOut;
        }
        if Instant::now() >= deadline {
            println!(
                "  ⏱ {} [{:?}] terminal member `{}` made no progress in {}s — freeing the slot; will retry.",
                req.id,
                req.status,
                member.id,
                cfg.agent_timeout.as_secs()
            );
            break TurnOutcome::TimedOut;
        }
    };
    // The TUI's output can't be streamed like subprocess stdout — snapshot the
    // visible pane into the member log instead.
    if let Some(log) = open_member_log(&cfg.workspace, &member.id, &format!("terminal {}", req.id)) {
        use std::io::Write;
        let mut log = log;
        let _ = writeln!(log, "{}", terminal::capture_pane(&name));
    }
    outcome
}

/// Create (or revive) the member's resident tmux session. The conversation id
/// doubles as the Claude Code session id, so a pane that died — reboot, crash,
/// manual kill — comes back with `--resume` and keeps its memory.
fn ensure_member_terminal(cfg: &Cfg, member: &TeamMember, name: &str) -> anyhow::Result<()> {
    let session_id = get_or_create_session(&cfg.workspace, &member.id);
    let resume = terminal::claude_session_exists(&session_id);
    let cmd = terminal::launch_command(&member.terminal_cmd, &session_id, resume);
    let created = terminal::ensure_session(name, &member.id, &cfg.project, &cfg.workspace, &cmd)?;
    if created {
        println!(
            "  ⌨ member `{}` resident terminal started ({}) — attach with: tmux attach -t {name}",
            member.id,
            if resume { "resumed session" } else { "new session" }
        );
    }
    // Wait for the CLI to actually accept input, answering one-time startup
    // dialogs (bypass-permissions acceptance, folder trust) on the way. A
    // blind sleep pastes the prompt into a dialog, where Enter picks "No,
    // exit" and kills the pane — planners then "time out" doing nothing.
    if !terminal::await_ready(name, Duration::from_secs(60)) {
        anyhow::bail!(
            "member `{}`'s terminal exited during startup — check `sh {}/.team-terminal/{name}.sh` by hand",
            member.id,
            cfg.workspace.display()
        );
    }
    Ok(())
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

/// The Stop-hook command dropped into `.claude/settings.json`. When a member's
/// Claude Code finishes a turn it runs this, touching that member's turn marker
/// so the orchestrator wakes the instant the agent stops — instead of polling to
/// a fixed timeout. Env vars are set per-pane by `ensure_session`; the guard
/// makes it a no-op for a human running Claude Code here outside forge.
const STOP_HOOK_CMD: &str = "[ -n \"$FORGE_MEMBER\" ] && [ -n \"$FORGE_WORKSPACE_DIR\" ] && \
     { mkdir -p \"$FORGE_WORKSPACE_DIR/.team-terminal\"; date +%s > \"$FORGE_WORKSPACE_DIR/.team-terminal/$FORGE_MEMBER.turn\"; } || true";

/// Install the turn-marker Stop hook into `<project>/.claude/settings.json`,
/// merging so a user's existing settings/hooks are preserved and re-runs are
/// idempotent (the hook is keyed by its exact command string).
fn ensure_stop_hook(project: &Path) -> anyhow::Result<()> {
    let dir = project.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    let mut cfg: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if cfg.get("hooks").is_none() {
        cfg["hooks"] = json!({});
    }
    let stop = cfg["hooks"]["Stop"].as_array().cloned().unwrap_or_default();
    // Already installed? (idempotent — match our exact command string.)
    let present = stop.iter().any(|group| {
        group["hooks"].as_array().map(|hs| {
            hs.iter().any(|h| h["command"].as_str() == Some(STOP_HOOK_CMD))
        }).unwrap_or(false)
    });
    if !present {
        let mut stop = stop;
        stop.push(json!({ "hooks": [{ "type": "command", "command": STOP_HOOK_CMD }] }));
        cfg["hooks"]["Stop"] = json!(stop);
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_stop_hook_merges_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        // Pre-seed a user settings.json with unrelated keys + their own Stop hook.
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        std::fs::write(
            proj.join(".claude/settings.json"),
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        ).unwrap();

        ensure_stop_hook(proj).unwrap();
        ensure_stop_hook(proj).unwrap(); // second call must not duplicate

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(proj.join(".claude/settings.json")).unwrap()).unwrap();
        // User's unrelated key + their hook are preserved.
        assert_eq!(v["model"], json!("opus"));
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        let cmds: Vec<&str> = stop.iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str())
            .collect();
        assert!(cmds.contains(&"echo mine"), "user hook preserved: {cmds:?}");
        // Ours is present exactly once despite two calls.
        assert_eq!(cmds.iter().filter(|c| **c == STOP_HOOK_CMD).count(), 1, "{cmds:?}");
    }

    #[test]
    fn test_member_for_pause_semantics() {
        let team = team::default_team();
        let none = HashSet::new();
        let eng_paused: HashSet<String> = ["engineer".to_string()].into();

        // Unpaused: engineer takes open work.
        assert!(matches!(
            member_for(&team, &none, RequestStatus::Open, "req-1"),
            Pick::Member(m, false) if m.id == "engineer"
        ));
        // Paused sole stage member: the request is held — no reroute, no cover.
        assert!(matches!(member_for(&team, &eng_paused, RequestStatus::Open, "req-1"), Pick::Held));
        // Other stages unaffected.
        assert!(matches!(
            member_for(&team, &eng_paused, RequestStatus::Review, "req-1"),
            Pick::Member(m, false) if m.id == "reviewer"
        ));
    }

    #[test]
    fn test_member_for_covering_lead_respects_pause() {
        let mut team = team::default_team();
        team.members.retain(|m| m.stage != Stage::Qa); // gap: nobody for qa
        let none = HashSet::new();
        // Lead (coordinator) covers the gap…
        assert!(matches!(
            member_for(&team, &none, RequestStatus::Qa, "req-1"),
            Pick::Member(m, true) if m.id == "coordinator"
        ));
        // …unless the Lead itself is paused — then the request waits.
        let lead_paused: HashSet<String> = ["coordinator".to_string()].into();
        assert!(matches!(member_for(&team, &lead_paused, RequestStatus::Qa, "req-1"), Pick::Held));
    }

    #[test]
    fn test_member_for_multi_member_stage_skips_paused() {
        let mut team = team::default_team();
        let mut e2 = team.members.iter().find(|m| m.id == "engineer").unwrap().clone();
        e2.id = "engineer-2".into();
        team.members.push(e2);
        // With one engineer paused, every open request lands on the other.
        let one_paused: HashSet<String> = ["engineer".to_string()].into();
        for req in ["req-a", "req-b", "req-c"] {
            assert!(matches!(
                member_for(&team, &one_paused, RequestStatus::Open, req),
                Pick::Member(m, false) if m.id == "engineer-2"
            ));
        }
    }
}
