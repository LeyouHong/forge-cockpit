//! Terminal-resident members — each one a long-lived tmux session running an
//! interactive CLI coding agent (Claude Code by default) that authenticates
//! with the CLI's own login (e.g. a Claude subscription), not a provider API
//! key.
//!
//! The orchestrator drives the member the way a human would: it pastes a
//! prompt into the pane and presses Enter, then watches the board/bus for
//! progress (the agent submits its work through the workspace MCP tools, which
//! Claude Code picks up from `<project>/.mcp.json`). The session survives
//! between tasks — `tmux attach -t <name>` drops a human into the member's
//! live terminal at any time.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bstr::ByteSlice;

/// Default terminal command when a member leaves `terminal_cmd` empty.
/// Unattended operation needs the permission prompts off, otherwise the first
/// tool call stalls the pipeline waiting for a human; set `terminal_cmd` to
/// `claude` on the member to keep prompts on instead. [`await_ready`] answers
/// the one-time acceptance dialog this flag triggers.
pub const DEFAULT_TERMINAL_CMD: &str = "claude --dangerously-skip-permissions";

/// tmux session name for a member: `forge-team-<project-tag>-<id>`. The project
/// tag scopes the session to one project so two projects that share a member id
/// (e.g. both have a `pm`) never collide on `forge-team-<id>` — reusing another
/// project's resident session silently runs the agent against the wrong board.
/// The id is already validated to letters/digits/-/_ by `validate_team`, which
/// tmux accepts verbatim.
pub fn session_name(project: &Path, member_id: &str) -> String {
    format!("forge-team-{}-{member_id}", project_tag(project))
}

/// A short, stable, tmux-safe tag identifying a project directory. Combines the
/// readable basename with a hash of the full path so two different directories
/// that happen to share a basename still get distinct tags. Stable across runs
/// (`DefaultHasher` uses fixed keys), so a project always maps to the same
/// session name and its resident terminals are found again on the next run.
fn project_tag(project: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut h);
    let hash = h.finish() & 0xff_ffff;
    let base: String = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(24)
        .collect();
    if base.is_empty() {
        format!("{hash:06x}")
    } else {
        format!("{base}-{hash:06x}")
    }
}

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    // Optional private socket. Unset in production (identical to the default
    // server); tests set it to a throwaway socket so they never touch — or get
    // reaped alongside — the user's real `forge-team-*` sessions.
    if let Ok(sock) = std::env::var("FORGE_TMUX_SOCKET") {
        if !sock.trim().is_empty() {
            c.args(["-L", sock.trim()]);
        }
    }
    c
}

/// Is tmux installed at all? Checked once at startup so a missing binary is a
/// clear configuration error instead of a per-request spawn failure.
pub fn tmux_available() -> bool {
    tmux().arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn has_session(name: &str) -> bool {
    tmux()
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// How long a resident member session may sit unused before [`reap_idle_sessions`]
/// reclaims it — the hygiene half of the resident model (aiwatching/forge reaps
/// idle terminals the same way). A session in active use streams pane output,
/// which keeps tmux `session_activity` fresh, so only genuinely dormant sessions
/// (finished, or never picked up) age out. Nothing is lost: the next request
/// recreates the session and `--resume` restores the agent's conversation.
pub const SESSION_IDLE: Duration = Duration::from_secs(4 * 60 * 60);

/// Kill every `forge-team-*` tmux session whose last activity is older than
/// `idle`, returning the names reaped. Idle is measured from tmux
/// `session_activity` (last pane output), so a working agent is never touched.
/// Scoped to the `forge-team-` prefix (unrelated user sessions are safe) but NOT
/// to one project — this is also how stale sessions from other projects or older
/// runs get cleaned up. Best-effort: a missing server or a failed kill just
/// yields fewer names; the next sweep retries.
pub fn reap_idle_sessions(idle: Duration) -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut reaped = Vec::new();
    for (name, activity) in list_forge_sessions() {
        // A pane with no readable activity timestamp counts as "just now" (not idle).
        if now.saturating_sub(activity.unwrap_or(now)) >= idle.as_secs() {
            let _ = kill_session(&name);
            reaped.push(name);
        }
    }
    reaped
}

/// Tear down every resident session for one project (`forge-team-<tag>-*`),
/// regardless of idle time — the explicit "I'm done with this team" action, the
/// counterpart to spawning it. Returns the names killed. Scoped by the project
/// tag so tearing down one project leaves other projects' teams running.
pub fn kill_project_sessions(project: &Path) -> Vec<String> {
    let prefix = format!("forge-team-{}-", project_tag(project));
    let mut killed = Vec::new();
    for (name, _) in list_forge_sessions() {
        if name.starts_with(&prefix) {
            let _ = kill_session(&name);
            killed.push(name);
        }
    }
    killed
}

fn kill_session(name: &str) {
    let _ = tmux().args(["kill-session", "-t", name]).output();
}

/// All `forge-team-*` sessions as `(name, last-activity-epoch-secs)`; the
/// activity is `None` when tmux reports no readable timestamp. Best-effort: a
/// missing server or tmux error yields an empty list.
fn list_forge_sessions() -> Vec<(String, Option<u64>)> {
    let out = match tmux()
        .args(["list-sessions", "-F", "#{session_name} #{session_activity}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(), // no server / no sessions / tmux error
    };
    out.stdout
        .to_str_lossy()
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let (name, activity) = (it.next()?, it.next());
            name.starts_with("forge-team-")
                .then(|| (name.to_string(), activity.and_then(|a| a.parse().ok())))
        })
        .collect()
}

/// Claude Code stores each session as `~/.claude/projects/<munged-cwd>/<id>.jsonl`.
/// Scanning for the id across project dirs tells us whether `--resume <id>`
/// can work or the session must be created fresh with `--session-id <id>`.
pub fn claude_session_exists(session_id: &str) -> bool {
    let base = home_dir().join(".claude").join("projects");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return false;
    };
    let file = format!("{session_id}.jsonl");
    entries.flatten().any(|d| d.path().join(&file).exists())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The command line a member's terminal runs. For a claude-family command the
/// per-member conversation id is appended so the session's memory survives
/// tmux restarts (`--resume` when the session already exists on disk,
/// `--session-id` to create it); other CLIs get the base command as-is.
pub fn launch_command(base_cmd: &str, session_id: &str, resume: bool) -> String {
    let base = if base_cmd.trim().is_empty() { DEFAULT_TERMINAL_CMD } else { base_cmd.trim() };
    let is_claude = base.split_whitespace().next().map(|w| w.contains("claude")).unwrap_or(false);
    if !is_claude {
        return base.to_string();
    }
    if resume {
        format!("{base} --resume {session_id}")
    } else {
        format!("{base} --session-id {session_id}")
    }
}

/// Ensure the member's resident session exists, (re)creating it if needed.
/// Returns true when a new session was created (caller should allow the TUI a
/// startup beat before injecting). The launch goes through a script file to
/// sidestep tmux argument quoting/truncation.
pub fn ensure_session(name: &str, project: &Path, workspace: &Path, cmd: &str) -> Result<bool> {
    if has_session(name) {
        return Ok(false);
    }
    let dir = workspace.join(".team-terminal");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncd {}\nexec {cmd}\n", shell_quote(&project.to_string_lossy())),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    let out = tmux()
        .args(["new-session", "-d", "-s", name, "-c"])
        .arg(project)
        .arg("sh")
        .arg(&script)
        .output()
        .context("spawn tmux new-session")?;
    if !out.status.success() {
        bail!("tmux new-session failed: {}", out.stderr.to_str_lossy().trim());
    }
    Ok(true)
}

/// Press a single key (tmux send-keys) — used to answer startup dialogs.
pub fn send_key(name: &str, key: &str) {
    let _ = tmux().args(["send-keys", "-t", name, key]).output();
}

/// Interrupt the agent's current turn without killing the session: sends the
/// CLI's "stop generating" key (Escape, which Claude Code uses to halt the
/// running turn). The resident session and its conversation survive — the agent
/// just stops what it's doing and returns to the prompt. Sent twice because a
/// single Escape can be swallowed while the TUI is mid-render.
pub fn interrupt(name: &str) {
    send_key(name, "Escape");
    std::thread::sleep(Duration::from_millis(120));
    send_key(name, "Escape");
}

/// Wait until the pane's CLI is ready for input, answering the well-known
/// one-time startup dialogs on the way. Blind sleeps are how prompts get
/// pasted into a dialog — where Enter picks the DEFAULT answer ("No, exit"
/// on the bypass-permissions screen), killing the pane before it ever works.
///
/// Handled dialogs (both persist their answer, so they are one-time):
///   - `--dangerously-skip-permissions` acceptance → select "Yes, I accept"
///   - folder trust ("Do you trust the files…")    → select "Yes, proceed"
///
/// Readiness = the idle footer bar is up and no dialog is on screen. On
/// timeout we proceed anyway (the CLI may be a non-claude command with no
/// known markers).
pub fn await_ready(name: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !has_session(name) {
            return false; // pane died (bad command, or a dialog answered "exit")
        }
        let pane = capture_pane(name);
        // Dialogs come in either order and each shows a numbered menu; answer
        // the accepting option, then confirm.
        let dialog = if pane.contains("Bypass Permissions mode") {
            Some("2") // "Yes, I accept"
        } else if pane.contains("Do you trust the files") {
            Some("1") // "Yes, I trust this folder"
        } else {
            None
        };
        match dialog {
            Some(key) => {
                send_key(name, key);
                std::thread::sleep(Duration::from_millis(400));
                send_key(name, "Enter");
            }
            // Footer hint = the prompt box is live and accepting input.
            None if pane.contains("shift+tab to cycle") || pane.contains("? for shortcuts") => {
                return true;
            }
            None => {}
        }
        if std::time::Instant::now() >= deadline {
            return true; // unknown CLI or slow start — let the caller try
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

/// Paste `text` into the member's pane and submit it. The pause between paste
/// and Enter is load-bearing: a TUI needs a beat to ingest the buffer, else
/// the Enter lands first and the prompt sits un-submitted.
pub fn send_text(name: &str, workspace: &Path, text: &str) -> Result<()> {
    let dir = workspace.join(".team-terminal");
    std::fs::create_dir_all(&dir)?;
    let buf_file = dir.join(format!("{name}.inject"));
    std::fs::write(&buf_file, text)?;
    let buf = format!("forgebuf-{name}");
    run_ok(tmux().args(["load-buffer", "-b", &buf]).arg(&buf_file), "tmux load-buffer")?;
    run_ok(tmux().args(["paste-buffer", "-d", "-b", &buf, "-t", name]), "tmux paste-buffer")?;
    std::thread::sleep(Duration::from_millis(400));
    run_ok(tmux().args(["send-keys", "-t", name, "Enter"]), "tmux send-keys")?;
    Ok(())
}

/// Snapshot the visible pane content (for the member log — the TUI's output
/// can't be captured as a stream the way subprocess stdout can).
pub fn capture_pane(name: &str) -> String {
    tmux()
        .args(["capture-pane", "-p", "-t", name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout.to_str_lossy().trim_end().to_string())
        .unwrap_or_default()
}

fn run_ok(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| what.to_string())?;
    if !out.status.success() {
        bail!("{what} failed: {}", out.stderr.to_str_lossy().trim());
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// tmux integration tests share one process, and `reap_idle_sessions` sweeps
    /// every `forge-team-*` session — so they must not run concurrently, and must
    /// not run against the user's real default server (they'd reap live sessions).
    /// The lock serializes them; [`TmuxSandbox`] points them at a throwaway
    /// socket. `unwrap_or_else` ignores poisoning so one failure doesn't cascade.
    static TMUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Redirects `tmux()` to a private, per-test socket for the guard's lifetime,
    /// then kills that server on drop — so tests never see or reap real sessions.
    /// Must be held together with `TMUX_TEST_LOCK` (the socket env is process-wide).
    struct TmuxSandbox(String);
    impl TmuxSandbox {
        fn new() -> Self {
            let sock = format!("forge-test-{}", std::process::id());
            // Safe: tmux tests are serialized by TMUX_TEST_LOCK, and only tmux()
            // reads this var — nothing else in the process does.
            unsafe { std::env::set_var("FORGE_TMUX_SOCKET", &sock) };
            TmuxSandbox(sock)
        }
    }
    impl Drop for TmuxSandbox {
        fn drop(&mut self) {
            let _ = Command::new("tmux").args(["-L", &self.0, "kill-server"]).output();
            unsafe { std::env::remove_var("FORGE_TMUX_SOCKET") };
        }
    }

    #[test]
    fn test_session_name() {
        // Project-scoped: `forge-team-<project-tag>-<id>`, still under the
        // `forge-team-` prefix the web terminal bridge validates.
        let name = session_name(Path::new("/tmp/webgames"), "engineer");
        assert!(name.starts_with("forge-team-webgames-"), "{name}");
        assert!(name.ends_with("-engineer"), "{name}");
        // Stable across calls, and distinct per project (even same basename).
        assert_eq!(name, session_name(Path::new("/tmp/webgames"), "engineer"));
        assert_ne!(name, session_name(Path::new("/other/webgames"), "engineer"));
    }

    #[test]
    fn test_reap_idle_sessions_scopes_and_ages() {
        if !tmux_available() {
            return; // CI without tmux — the real path is covered by the e2e run
        }
        let _serial = TMUX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = TmuxSandbox::new();
        let mine = "forge-team-reaptest-a1b2c3-pm";
        let theirs = "not-forge-reaptest"; // unrelated user session must survive
        for n in [mine, theirs] {
            let _ = tmux().args(["kill-session", "-t", n]).output();
            let out = tmux().args(["new-session", "-d", "-s", n, "sh", "-c", "sleep 30"]).output().unwrap();
            assert!(out.status.success());
        }
        // idle=1h: freshly-created sessions are NOT idle → nothing reaped.
        assert!(reap_idle_sessions(Duration::from_secs(3600)).is_empty());
        assert!(has_session(mine) && has_session(theirs));
        // idle=0: everything qualifies, but only the forge-team-* one is in scope.
        let reaped = reap_idle_sessions(Duration::from_secs(0));
        assert_eq!(reaped, vec![mine.to_string()]);
        assert!(!has_session(mine), "idle forge-team session should be reaped");
        assert!(has_session(theirs), "unrelated session must be left alone");
        let _ = tmux().args(["kill-session", "-t", theirs]).output();
    }

    #[test]
    fn test_kill_project_sessions_is_project_scoped() {
        if !tmux_available() {
            return; // CI without tmux — the real path is covered by the e2e run
        }
        let _serial = TMUX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = TmuxSandbox::new();
        let a = Path::new("/tmp/proj-a");
        let b = Path::new("/tmp/proj-b");
        let a_pm = session_name(a, "pm");
        let b_pm = session_name(b, "pm");
        for n in [&a_pm, &b_pm] {
            let _ = kill_session(n);
            let out = tmux().args(["new-session", "-d", "-s", n, "sh", "-c", "sleep 30"]).output().unwrap();
            assert!(out.status.success());
        }
        // Tearing down project A leaves project B's session alive.
        let killed = kill_project_sessions(a);
        assert_eq!(killed, vec![a_pm.clone()]);
        assert!(!has_session(&a_pm), "project A session should be gone");
        assert!(has_session(&b_pm), "project B session must survive");
        let _ = kill_session(&b_pm);
    }

    #[test]
    fn test_launch_command_claude_fresh_vs_resume() {
        let id = "0196f0c8-aaaa-bbbb-cccc-1234567890ab";
        assert_eq!(
            launch_command("", id, false),
            format!("{DEFAULT_TERMINAL_CMD} --session-id {id}")
        );
        assert_eq!(launch_command("claude", id, true), format!("claude --resume {id}"));
    }

    #[test]
    fn test_launch_command_non_claude_gets_no_session_flags() {
        assert_eq!(launch_command("codex --full-auto", "id-1", true), "codex --full-auto");
    }

    /// The dialog gauntlet, simulated: a pane that shows the trust dialog, then
    /// the bypass dialog, then the idle footer — each advancing only when the
    /// right key arrives. Guards the ordering assumption (either dialog can come
    /// first) and the readiness marker.
    #[test]
    fn test_await_ready_answers_dialogs_then_sees_footer() {
        if !tmux_available() {
            return; // CI without tmux — the real path is covered by the e2e run
        }
        let _serial = TMUX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = TmuxSandbox::new();
        let name = "forge-team-awaittest";
        let _ = tmux().args(["kill-session", "-t", name]).output();
        // A pane whose content walks the three states as keys arrive.
        let script = "printf 'Do you trust the files in this folder?\n 1. Yes\n'; read a;                       printf 'WARNING: Claude Code running in Bypass Permissions mode\n 2. Yes, I accept\n'; read b;                       printf 'shift+tab to cycle\n'; sleep 30";
        let out = tmux().args(["new-session", "-d", "-s", name, "sh", "-c", script]).output().unwrap();
        assert!(out.status.success());
        let ready = await_ready(name, Duration::from_secs(20));
        let pane = capture_pane(name);
        let _ = tmux().args(["kill-session", "-t", name]).output();
        assert!(ready, "await_ready should reach the footer; pane was:\n{pane}");
        assert!(pane.contains("shift+tab to cycle"), "pane never reached idle:\n{pane}");
    }

    #[test]
    fn test_shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
