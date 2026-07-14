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

/// Default terminal command when a member sets `terminal: true` without a
/// custom `terminal_cmd`. Unattended operation needs the permission prompts
/// off, otherwise the first tool call stalls the pipeline waiting for a human;
/// set `terminal_cmd: "claude"` on the member to keep prompts on instead.
pub const DEFAULT_TERMINAL_CMD: &str = "claude --dangerously-skip-permissions";

/// tmux session name for a member: `forge-team-<id>` (id is already validated
/// to letters/digits/-/_ by `validate_team`, which tmux accepts verbatim).
pub fn session_name(member_id: &str) -> String {
    format!("forge-team-{member_id}")
}

fn tmux() -> Command {
    Command::new("tmux")
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

    #[test]
    fn test_session_name() {
        assert_eq!(session_name("engineer"), "forge-team-engineer");
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

    #[test]
    fn test_shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
