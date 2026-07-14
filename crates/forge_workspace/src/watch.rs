//! Watch manager — autonomous monitoring that feeds the board.
//!
//! A watch is a piece of data (`<workspace>/.watches.json`): *look at this
//! thing every N seconds, and when it changes, act*. The orchestrator
//! evaluates watches on its poll loop; state (fingerprints) lives next to the
//! config in `.watch-state.json`, so restarts don't re-fire old changes.
//!
//! Three kinds of target:
//!   - `files`   — a path under the project; fires on added/changed/removed
//!                 files (optional `pattern` filter, `*.rs`-style suffix or
//!                 substring)
//!   - `git`     — HEAD plus working-tree status; fires on commits and
//!                 dirty-tree transitions
//!   - `command` — a shell command's stdout; fires when the output changes
//!
//! Two actions, mapping to how this workspace already routes work:
//!   - `request` — create a request on the board; the normal stage pipeline
//!                 picks it up (the team *analyzes the change* autonomously)
//!   - `alert`   — send a ticket to the human/alert inbox (a person decides)
//!
//! The first evaluation of a watch only records a baseline — a watch never
//! fires on what the world already looked like when it was created.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bstr::ByteSlice;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchKind {
    Files,
    Git,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchAction {
    /// Create a board request — the team handles the change autonomously.
    Request,
    /// Ticket to the alert inbox — a human decides what (if anything) to do.
    Alert,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    pub kind: WatchKind,
    /// files: path relative to the project (dir or file). git: unused.
    /// command: a `sh -c` command line run in the project directory.
    #[serde(default)]
    pub target: String,
    /// files only: `*.ext` suffix filter or plain substring; empty = all.
    #[serde(default)]
    pub pattern: String,
    pub action: WatchAction,
    /// Extra context prepended to the request/alert body — what the team
    /// should DO about a change (e.g. "re-run the test suite and fix breaks").
    #[serde(default)]
    pub brief: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_interval() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WatchState {
    /// Kind-specific fingerprint of the last-seen world.
    #[serde(default)]
    fingerprint: serde_json::Value,
    #[serde(default)]
    last_check: u64,
    #[serde(default)]
    last_fired: u64,
}

/// A change a watch detected, ready to be routed by the caller.
#[derive(Debug, Clone)]
pub struct Fired {
    pub watch_id: String,
    pub action: WatchAction,
    /// One-line what-changed headline.
    pub headline: String,
    /// Fuller change summary (file lists, output excerpts) plus the brief.
    pub body: String,
}

fn watches_path(workspace: &Path) -> PathBuf {
    workspace.join(".watches.json")
}
fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".watch-state.json")
}

pub fn load_watches(workspace: &Path) -> Vec<Watch> {
    std::fs::read_to_string(watches_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_watches(workspace: &Path, watches: &[Watch]) -> Result<()> {
    validate_watches(watches)?;
    std::fs::create_dir_all(workspace)?;
    std::fs::write(watches_path(workspace), serde_json::to_string_pretty(watches)?)?;
    Ok(())
}

pub fn validate_watches(watches: &[Watch]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for w in watches {
        let id = w.id.trim();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("invalid watch id `{}` (use letters/digits/-/_)", w.id);
        }
        if !seen.insert(id.to_string()) {
            bail!("duplicate watch id `{id}`");
        }
        match w.kind {
            WatchKind::Files | WatchKind::Command if w.target.trim().is_empty() => {
                bail!("watch `{id}`: {:?} needs a target", w.kind)
            }
            _ => {}
        }
        if w.interval_secs == 0 {
            bail!("watch `{id}`: interval_secs must be > 0");
        }
    }
    Ok(())
}

fn load_state(workspace: &Path) -> BTreeMap<String, WatchState> {
    std::fs::read_to_string(state_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(workspace: &Path, state: &BTreeMap<String, WatchState>) {
    let _ = std::fs::create_dir_all(workspace);
    let _ = std::fs::write(
        state_path(workspace),
        serde_json::to_string_pretty(state).unwrap_or_default(),
    );
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Evaluate all due watches once. Baselines are recorded silently; real
/// changes come back as [`Fired`] entries for the caller to route.
pub fn tick(project: &Path, workspace: &Path) -> Vec<Fired> {
    let watches = load_watches(workspace);
    if watches.is_empty() {
        return Vec::new();
    }
    let mut state = load_state(workspace);
    let now = now_epoch();
    let mut fired = Vec::new();
    let mut dirty = false;

    for w in watches.iter().filter(|w| w.enabled) {
        let st = state.entry(w.id.clone()).or_default();
        if now.saturating_sub(st.last_check) < w.interval_secs {
            continue;
        }
        st.last_check = now;
        dirty = true;
        let current = match fingerprint(w, project) {
            Ok(v) => v,
            Err(e) => {
                // A broken target (bad path, failing command spawn) shouldn't
                // fire or thrash — note it once at trace level and move on.
                tracing_note(&w.id, &e);
                continue;
            }
        };
        if st.fingerprint.is_null() {
            st.fingerprint = current; // first look = baseline, never fires
            continue;
        }
        if st.fingerprint != current {
            let (headline, detail) = describe(w, &st.fingerprint, &current);
            st.fingerprint = current;
            st.last_fired = now;
            let mut body = String::new();
            if !w.brief.trim().is_empty() {
                body.push_str(w.brief.trim());
                body.push_str("\n\n");
            }
            body.push_str(&detail);
            fired.push(Fired { watch_id: w.id.clone(), action: w.action, headline, body });
        }
    }
    if dirty {
        save_state(workspace, &state);
    }
    fired
}

fn tracing_note(id: &str, e: &anyhow::Error) {
    eprintln!("  ! watch `{id}`: {e:#}");
}

fn fingerprint(w: &Watch, project: &Path) -> Result<serde_json::Value> {
    match w.kind {
        WatchKind::Files => files_fingerprint(project, &w.target, &w.pattern),
        WatchKind::Git => git_fingerprint(project),
        WatchKind::Command => command_fingerprint(project, &w.target),
    }
}

// ── files ──

fn pattern_match(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    name.contains(pattern)
}

/// rel path → "mtime_ms:len" for every file in the watched subtree.
fn files_fingerprint(project: &Path, target: &str, pattern: &str) -> Result<serde_json::Value> {
    let root = project.join(target);
    if !root.exists() {
        bail!("path `{target}` does not exist under the project");
    }
    let mut map = BTreeMap::new();
    walk_files(&root, &root, pattern, 0, &mut map)?;
    Ok(serde_json::to_value(map)?)
}

fn walk_files(
    root: &Path,
    dir: &Path,
    pattern: &str,
    depth: u32,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    let meta = std::fs::metadata(dir)?;
    if meta.is_file() {
        record_file(root, dir, out);
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let _ = walk_files(root, &path, pattern, depth + 1, out);
        } else if ft.is_file() && pattern_match(pattern, &name) {
            record_file(root, &path, out);
        }
    }
    Ok(())
}

fn record_file(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
    let rel = if rel.is_empty() { path.to_string_lossy().into_owned() } else { rel };
    out.insert(rel, format!("{mtime}:{}", meta.len()));
}

// ── git ──

fn git_fingerprint(project: &Path) -> Result<serde_json::Value> {
    let head = run_capture(project, "git", &["rev-parse", "HEAD"], Duration::from_secs(10))?;
    let status =
        run_capture(project, "git", &["status", "--porcelain"], Duration::from_secs(10))?;
    Ok(serde_json::json!({ "head": head.trim(), "status": status.trim() }))
}

// ── command ──

fn command_fingerprint(project: &Path, cmd: &str) -> Result<serde_json::Value> {
    let out = run_capture(project, "sh", &["-c", cmd], Duration::from_secs(30))?;
    Ok(serde_json::Value::String(out.trim_end().to_string()))
}

/// Run a command with a hard deadline (a wedged watch command must not stall
/// the orchestrator's poll loop forever).
fn run_capture(cwd: &Path, prog: &str, args: &[&str], timeout: Duration) -> Result<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {prog}"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("`{prog}` timed out after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => bail!("wait {prog}: {e}"),
        }
    }
    let mut buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_end(&mut buf);
    }
    Ok(buf.to_str_lossy().into_owned())
}

// ── change description ──

fn describe(w: &Watch, old: &serde_json::Value, new: &serde_json::Value) -> (String, String) {
    match w.kind {
        WatchKind::Files => {
            let empty = BTreeMap::new();
            let o: BTreeMap<String, String> =
                serde_json::from_value(old.clone()).unwrap_or_else(|_| empty.clone());
            let n: BTreeMap<String, String> = serde_json::from_value(new.clone()).unwrap_or(empty);
            let added: Vec<&String> = n.keys().filter(|k| !o.contains_key(*k)).collect();
            let removed: Vec<&String> = o.keys().filter(|k| !n.contains_key(*k)).collect();
            let changed: Vec<&String> =
                n.iter().filter(|(k, v)| o.get(*k).is_some_and(|ov| ov != *v)).map(|(k, _)| k).collect();
            let headline = format!(
                "{}: {} added, {} changed, {} removed",
                w.target,
                added.len(),
                changed.len(),
                removed.len()
            );
            let list = |label: &str, v: &[&String]| {
                if v.is_empty() {
                    String::new()
                } else {
                    format!(
                        "{label}:\n{}\n",
                        v.iter().take(20).map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n")
                    )
                }
            };
            let detail = format!(
                "Watched path `{}` changed.\n{}{}{}",
                w.target,
                list("Added", &added),
                list("Changed", &changed),
                list("Removed", &removed)
            );
            (headline, detail)
        }
        WatchKind::Git => {
            let (oh, nh) = (old["head"].as_str().unwrap_or(""), new["head"].as_str().unwrap_or(""));
            let ns = new["status"].as_str().unwrap_or("");
            let headline = if oh != nh {
                format!("git: HEAD moved {} → {}", &oh[..oh.len().min(9)], &nh[..nh.len().min(9)])
            } else {
                "git: working tree changed".to_string()
            };
            let detail = format!(
                "{headline}\n\nCurrent `git status --porcelain`:\n{}",
                if ns.is_empty() { "(clean)" } else { ns }
            );
            (headline, detail)
        }
        WatchKind::Command => {
            let n = new.as_str().unwrap_or("");
            let headline = format!("`{}` output changed", w.target);
            let excerpt: String = n.chars().take(1200).collect();
            let detail = format!("{headline}\n\nNew output (first 1200 chars):\n```\n{excerpt}\n```");
            (headline, detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn watch(id: &str, kind: WatchKind, target: &str) -> Watch {
        Watch {
            id: id.into(),
            kind,
            target: target.into(),
            pattern: String::new(),
            action: WatchAction::Request,
            brief: String::new(),
            interval_secs: 1,
            enabled: true,
        }
    }

    #[test]
    fn test_validate_rejects_bad_configs() {
        assert!(validate_watches(&[watch("", WatchKind::Git, "")]).is_err());
        assert!(validate_watches(&[watch("a", WatchKind::Files, "")]).is_err());
        assert!(
            validate_watches(&[watch("a", WatchKind::Git, ""), watch("a", WatchKind::Git, "")])
                .is_err()
        );
        assert!(validate_watches(&[watch("ok-1", WatchKind::Command, "true")]).is_ok());
    }

    #[test]
    fn test_files_watch_baseline_then_fire_on_change() {
        let proj = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(proj.path().join("src")).unwrap();
        std::fs::write(proj.path().join("src/a.rs"), "one").unwrap();
        save_watches(ws.path(), &[watch("w", WatchKind::Files, "src")]).unwrap();

        // First tick: baseline only.
        assert_eq!(tick(proj.path(), ws.path()).len(), 0);

        // No change → quiet. (interval is 1s; backdate last_check instead of sleeping)
        backdate(ws.path(), "w");
        assert_eq!(tick(proj.path(), ws.path()).len(), 0);

        // Add + modify → one Fired with both reflected.
        std::fs::write(proj.path().join("src/b.rs"), "new").unwrap();
        std::fs::write(proj.path().join("src/a.rs"), "one-changed").unwrap();
        backdate(ws.path(), "w");
        let fired = tick(proj.path(), ws.path());
        assert_eq!(fired.len(), 1);
        assert!(fired[0].headline.contains("1 added"), "{}", fired[0].headline);
        assert!(fired[0].body.contains("b.rs"));

        // Fired state persists — immediate re-tick is quiet.
        backdate(ws.path(), "w");
        assert_eq!(tick(proj.path(), ws.path()).len(), 0);
    }

    #[test]
    fn test_command_watch_fires_on_output_change() {
        let proj = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let marker = proj.path().join("marker");
        std::fs::write(&marker, "v1").unwrap();
        save_watches(ws.path(), &[watch("c", WatchKind::Command, "cat marker")]).unwrap();

        assert_eq!(tick(proj.path(), ws.path()).len(), 0); // baseline
        std::fs::write(&marker, "v2").unwrap();
        backdate(ws.path(), "c");
        let fired = tick(proj.path(), ws.path());
        assert_eq!(fired.len(), 1);
        assert!(fired[0].body.contains("v2"));
    }

    #[test]
    fn test_pattern_match_suffix_and_substring() {
        assert!(pattern_match("*.rs", "main.rs"));
        assert!(!pattern_match("*.rs", "main.ts"));
        assert!(pattern_match("test", "my_test_file.py"));
        assert!(pattern_match("", "anything"));
    }

    /// Rewind a watch's last_check so the next tick is due immediately.
    fn backdate(ws: &Path, id: &str) {
        let mut st = load_state(ws);
        if let Some(s) = st.get_mut(id) {
            s.last_check = 0;
        }
        save_state(ws, &st);
    }
}
