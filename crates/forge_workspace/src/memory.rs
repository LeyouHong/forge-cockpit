//! Per-agent structured memory — forgecode's take on aiwatching/forge's "Smith
//! Memory". Our members are resident Claude Code sessions that already keep raw
//! continuity across turns/runs via `--resume`, so this layer does NOT try to
//! reconstruct their reasoning. Instead it adds the *portable, human-visible,
//! structured* half: an agent records durable observations through the
//! `record_observation` MCP tool, and a progressive-disclosure recap is injected
//! into its prompt each turn (recent entries in full, older ones title-only).
//!
//! Storage: `<workspace>/.agent-memory/<member>.json` — plain disk state next to
//! the board and `.team-*.json`, so it survives restarts and a `/clear` of the
//! agent's own conversation, and a human can read it without touching `.jsonl`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Cap per member so the file (and the injected recap) can't grow without bound;
/// the oldest observations are dropped first.
const MAX_OBSERVATIONS: usize = 100;
/// The most recent N observations keep full detail in the prompt; older ones are
/// shown title-only (progressive disclosure).
const FULL_DETAIL_COUNT: usize = 12;

/// The six observation kinds, mirroring the reference. Unknown values are kept
/// verbatim (the MCP schema constrains them, but storage stays permissive).
pub const KINDS: [&str; 6] = ["decision", "bugfix", "feature", "refactor", "discovery", "change"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    /// Unix seconds.
    pub ts: u64,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Related request id, if the observation came out of one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMemory {
    pub member: String,
    /// Oldest first; the tail is the most recent work.
    pub observations: Vec<Observation>,
}

fn memory_dir(workspace: &Path) -> PathBuf {
    workspace.join(".agent-memory")
}

fn member_path(workspace: &Path, member: &str) -> PathBuf {
    memory_dir(workspace).join(format!("{member}.json"))
}

/// Normalize an agent name to its member id: agents call themselves `engineer-1`,
/// but memory is keyed by the member id (`engineer`) like the rest of the team
/// state. A bare id, or one whose suffix isn't numeric, is left untouched.
pub fn member_key(agent: &str) -> String {
    match agent.rsplit_once('-') {
        Some((base, num)) if !base.is_empty() && !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) => {
            base.to_string()
        }
        _ => agent.to_string(),
    }
}

pub fn load(workspace: &Path, member: &str) -> AgentMemory {
    std::fs::read_to_string(member_path(workspace, member))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| AgentMemory { member: member.to_string(), observations: Vec::new() })
}

fn save(workspace: &Path, mem: &AgentMemory) -> Result<()> {
    let dir = memory_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(member_path(workspace, &mem.member), serde_json::to_string_pretty(mem)?)?;
    Ok(())
}

/// Append an observation to `member`'s memory, pruning to the newest
/// [`MAX_OBSERVATIONS`]. Returns the stored observation (with its generated id).
pub fn record(
    workspace: &Path,
    member: &str,
    kind: &str,
    title: &str,
    facts: Vec<String>,
    files: Vec<String>,
    detail: &str,
    request: &str,
) -> Result<Observation> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let obs = Observation {
        id: format!("obs-{ts}-{:04x}", rand_suffix()),
        ts,
        kind: kind.trim().to_string(),
        title: title.trim().to_string(),
        facts,
        files,
        detail: detail.trim().to_string(),
        request: request.trim().to_string(),
    };
    let mut mem = load(workspace, member);
    mem.member = member.to_string();
    mem.observations.push(obs.clone());
    let n = mem.observations.len();
    if n > MAX_OBSERVATIONS {
        mem.observations.drain(0..n - MAX_OBSERVATIONS);
    }
    save(workspace, &mem)?;
    Ok(obs)
}

/// A tiny non-crypto id suffix — enough to disambiguate observations recorded in
/// the same second without pulling in the `rand` crate.
fn rand_suffix() -> u16 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    (h.finish() & 0xffff) as u16
}

fn icon(kind: &str) -> &'static str {
    match kind {
        "decision" => "🎯",
        "bugfix" => "🐛",
        "feature" => "✨",
        "refactor" => "♻️",
        "discovery" => "🔍",
        "change" => "✏️",
        _ => "•",
    }
}

/// Render a member's memory as a prompt block: the most recent
/// [`FULL_DETAIL_COUNT`] observations in full (facts/files/detail), older ones
/// title-only. Empty string when there's nothing to recall (so callers can
/// append unconditionally).
pub fn format_for_prompt(mem: &AgentMemory) -> String {
    if mem.observations.is_empty() {
        return String::new();
    }
    let n = mem.observations.len();
    let full_from = n.saturating_sub(FULL_DETAIL_COUNT);
    let mut s = String::from(
        "## Your memory\nDurable notes you recorded on earlier turns — build on them and \
         don't redo solved work. Record new ones with `record_observation`.\n",
    );
    for (i, o) in mem.observations.iter().enumerate() {
        if i >= full_from {
            s.push_str(&format!("- {} **{}** — {}\n", icon(&o.kind), o.kind, o.title));
            for f in &o.facts {
                s.push_str(&format!("    · {f}\n"));
            }
            if !o.files.is_empty() {
                s.push_str(&format!("    files: {}\n", o.files.join(", ")));
            }
            if !o.detail.is_empty() {
                s.push_str(&format!("    {}\n", o.detail));
            }
        } else {
            s.push_str(&format!("- {} {} _(earlier)_\n", icon(&o.kind), o.title));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_member_key_strips_numeric_suffix() {
        assert_eq!(member_key("engineer-1"), "engineer");
        assert_eq!(member_key("pm-1"), "pm");
        assert_eq!(member_key("engineer"), "engineer"); // already bare
        assert_eq!(member_key("code-review"), "code-review"); // non-numeric suffix kept
    }

    #[test]
    fn test_record_load_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        assert!(load(ws, "eng").observations.is_empty());
        for i in 0..(MAX_OBSERVATIONS + 5) {
            record(ws, "eng", "change", &format!("obs {i}"), vec![], vec![], "", "").unwrap();
        }
        let mem = load(ws, "eng");
        // Pruned to the cap, keeping the newest.
        assert_eq!(mem.observations.len(), MAX_OBSERVATIONS);
        assert_eq!(mem.observations.last().unwrap().title, format!("obs {}", MAX_OBSERVATIONS + 4));
        // A different member is isolated.
        assert!(load(ws, "qa").observations.is_empty());
    }

    #[test]
    fn test_format_progressive_disclosure() {
        assert_eq!(format_for_prompt(&AgentMemory::default()), "");
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        record(ws, "eng", "decision", "chose top-down projection", vec!["A* pathfinding".into()], vec!["index.html".into()], "detail here", "req-1").unwrap();
        let out = format_for_prompt(&load(ws, "eng"));
        assert!(out.contains("## Your memory"), "{out}");
        assert!(out.contains("chose top-down projection") && out.contains("A* pathfinding"), "{out}");
    }
}
