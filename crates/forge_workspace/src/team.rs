//! Team composition as data — `<workspace>/.team.json`.
//!
//! The team is no longer compiled into the orchestrator: it is a list of
//! **members** (id, display, stage, optional custom SOP, optional forge agent,
//! DAG `depends_on` edges) that the web canvas edits and
//! `forge-workspace-run` executes. When no file exists, [`default_team`]
//! yields the built-in six-role roster (pm → architect → coordinator →
//! engineer → reviewer → qa), so existing workspaces keep working unchanged.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Which part of the lifecycle a member works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Runs once, synchronously, when the team is started with a goal
    /// (PRD / design / board sanity — anything upstream of the pipeline).
    Plan,
    /// Works requests in `open` / `in_progress`.
    Implement,
    /// Works requests in `review`.
    Review,
    /// Works requests in `qa`.
    Qa,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    /// Stable identifier; also the agent name prefix (`<id>-1`).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    pub stage: Stage,
    /// Optional forge agent id (`forge -p --agent <id>`) — how a member gets
    /// its own model/persona. Empty → the default agent.
    #[serde(default)]
    pub agent: String,
    /// Custom SOP (markdown). Empty → the built-in SOP for well-known ids
    /// (pm, architect, coordinator, engineer, reviewer, qa), or a generic
    /// stage SOP for custom members.
    #[serde(default)]
    pub role_prompt: String,
    /// Upstream member ids (the canvas edges). Used for planning order and
    /// the topology snapshot every agent sees.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// When true, a human must approve each piece of work before this member
    /// is spawned for it (the orchestrator parks the request and alerts).
    #[serde(default)]
    pub requires_approval: bool,
    /// When true, this member is a resident terminal: a persistent tmux
    /// session running an interactive CLI agent (Claude Code by default, on
    /// the CLI's own subscription auth) that the orchestrator drives by
    /// injecting prompts. `tmux attach -t forge-team-<id>` joins it live.
    #[serde(default)]
    pub terminal: bool,
    /// Base command for the resident terminal. Empty → Claude Code with
    /// permission prompts off (unattended operation); session-resume flags are
    /// appended automatically for claude-family commands.
    #[serde(default)]
    pub terminal_cmd: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    pub members: Vec<TeamMember>,
    /// Canvas layout, keyed by member id (plus the synthetic `done` column).
    #[serde(default)]
    pub positions: BTreeMap<String, Pos>,
}

fn member(id: &str, name: &str, icon: &str, stage: Stage, depends_on: &[&str]) -> TeamMember {
    TeamMember {
        id: id.into(),
        name: name.into(),
        icon: icon.into(),
        stage,
        agent: String::new(),
        role_prompt: String::new(),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        requires_approval: false,
        terminal: false,
        terminal_cmd: String::new(),
    }
}

/// The built-in roster — used when `.team.json` doesn't exist.
pub fn default_team() -> TeamConfig {
    TeamConfig {
        members: vec![
            member("pm", "PM", "📋", Stage::Plan, &[]),
            member("architect", "Architect", "🏗️", Stage::Plan, &["pm"]),
            member("coordinator", "Coordinator", "🧭", Stage::Plan, &["architect"]),
            member("engineer", "Engineer", "🔨", Stage::Implement, &["coordinator"]),
            member("reviewer", "Reviewer", "🔍", Stage::Review, &["engineer"]),
            member("qa", "QA", "✅", Stage::Qa, &["reviewer"]),
        ],
        positions: BTreeMap::new(),
    }
}

/// Load the team config, falling back to the built-in roster.
pub fn load_team(workspace: &Path) -> TeamConfig {
    std::fs::read_to_string(workspace.join(".team.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<TeamConfig>(&s).ok())
        .filter(|c| !c.members.is_empty())
        .unwrap_or_else(default_team)
}

/// Persist the team config (validated).
pub fn save_team(workspace: &Path, cfg: &TeamConfig) -> Result<()> {
    validate_team(cfg)?;
    std::fs::create_dir_all(workspace)?;
    std::fs::write(workspace.join(".team.json"), serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Pause flags, persisted at `<workspace>/.team-paused.json` as
/// `{"<member-id>": true}`. A paused member is never scheduled new work
/// (in-flight work finishes normally); requests for its stage wait rather
/// than being rerouted or covered — pause means "hold", not "reassign".
fn paused_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(".team-paused.json")
}

pub fn load_paused(workspace: &Path) -> HashSet<String> {
    std::fs::read_to_string(paused_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, bool>>(&s).ok())
        .map(|m| m.into_iter().filter(|(_, v)| *v).map(|(k, _)| k).collect())
        .unwrap_or_default()
}

pub fn set_paused(workspace: &Path, member: &str, paused: bool) -> Result<()> {
    let mut map: BTreeMap<String, bool> = std::fs::read_to_string(paused_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if paused {
        map.insert(member.to_string(), true);
    } else {
        map.remove(member);
    }
    std::fs::create_dir_all(workspace)?;
    std::fs::write(paused_path(workspace), serde_json::to_string_pretty(&map)?)?;
    Ok(())
}

/// Reject configs the orchestrator can't run: duplicate/empty ids, unknown
/// `depends_on` targets, dependency cycles, or no implement-stage member.
pub fn validate_team(cfg: &TeamConfig) -> Result<()> {
    if cfg.members.is_empty() {
        bail!("team has no members");
    }
    let mut seen = HashSet::new();
    for m in &cfg.members {
        let id = m.id.trim();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("invalid member id `{}` (use letters/digits/-/_)", m.id);
        }
        if !seen.insert(id.to_string()) {
            bail!("duplicate member id `{id}`");
        }
    }
    let ids: HashSet<&str> = cfg.members.iter().map(|m| m.id.as_str()).collect();
    for m in &cfg.members {
        for d in &m.depends_on {
            if !ids.contains(d.as_str()) {
                bail!("member `{}` depends on unknown member `{d}`", m.id);
            }
            if d == &m.id {
                bail!("member `{}` depends on itself", m.id);
            }
        }
    }
    topo_order(&cfg.members).map_err(|c| anyhow::anyhow!("dependency cycle involving `{c}`"))?;
    if !cfg.members.iter().any(|m| m.stage == Stage::Implement) {
        bail!("team needs at least one implement-stage member (nobody would write code)");
    }
    Ok(())
}

/// Kahn topological order over `depends_on`; `Err(id)` names a cycle member.
/// Ties keep the authored order (stable for equal in-degree).
pub fn topo_order(members: &[TeamMember]) -> std::result::Result<Vec<TeamMember>, String> {
    let idx: HashMap<&str, usize> = members.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let mut indeg: Vec<usize> = members
        .iter()
        .map(|m| m.depends_on.iter().filter(|d| idx.contains_key(d.as_str())).count())
        .collect();
    let mut out = Vec::with_capacity(members.len());
    let mut done = vec![false; members.len()];
    while out.len() < members.len() {
        let Some(i) = (0..members.len()).find(|&i| !done[i] && indeg[i] == 0) else {
            let stuck = members.iter().enumerate().find(|(i, _)| !done[*i]).map(|(_, m)| m.id.clone());
            return Err(stuck.unwrap_or_default());
        };
        done[i] = true;
        out.push(members[i].clone());
        for (j, m) in members.iter().enumerate() {
            if !done[j] && m.depends_on.iter().any(|d| d == &members[i].id) {
                indeg[j] -= 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_default_team_is_valid_and_topo_ordered() {
        let cfg = default_team();
        validate_team(&cfg).unwrap();
        let order: Vec<String> = topo_order(&cfg.members).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(order, vec!["pm", "architect", "coordinator", "engineer", "reviewer", "qa"]);
    }

    #[test]
    fn test_load_falls_back_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        // no file → default
        assert_eq!(load_team(tmp.path()).members.len(), 6);
        // save custom → load returns it
        let mut cfg = default_team();
        cfg.members.push(member("engineer-2", "Engineer 2", "🔨", Stage::Implement, &["coordinator"]));
        save_team(tmp.path(), &cfg).unwrap();
        assert_eq!(load_team(tmp.path()).members.len(), 7);
    }

    #[test]
    fn test_pause_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_paused(tmp.path()).is_empty());
        set_paused(tmp.path(), "eng", true).unwrap();
        set_paused(tmp.path(), "qa", true).unwrap();
        let p = load_paused(tmp.path());
        assert!(p.contains("eng") && p.contains("qa"));
        set_paused(tmp.path(), "eng", false).unwrap();
        let p = load_paused(tmp.path());
        assert!(!p.contains("eng") && p.contains("qa"));
    }

    #[test]
    fn test_validate_rejects_bad_configs() {
        let mut dup = default_team();
        dup.members.push(member("pm", "PM2", "", Stage::Plan, &[]));
        assert!(validate_team(&dup).is_err());

        let mut unknown = default_team();
        unknown.members[0].depends_on = vec!["ghost".into()];
        assert!(validate_team(&unknown).is_err());

        let mut cycle = default_team();
        cycle.members[0].depends_on = vec!["architect".into()]; // pm ↔ architect
        assert!(validate_team(&cycle).is_err());

        let mut no_eng = default_team();
        no_eng.members.retain(|m| m.stage != Stage::Implement);
        assert!(validate_team(&no_eng).is_err());
    }
}
