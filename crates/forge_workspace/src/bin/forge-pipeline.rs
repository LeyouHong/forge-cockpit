//! Run a YAML workflow (a DAG of AI/shell tasks) to completion.
//!
//!   forge-pipeline run <flow.yaml> [--input k=v]... [--project DIR]
//!       [--workspace DIR] [--forge PATH] [--concurrent N]
//!       [--node-timeout-secs N] [--isolate-mcp] [--dry-run]
//!
//! Node prompts template `{{input.x}}`, `{{vars.y}}`, and
//! `{{nodes.<id>.outputs.<name>}}`; ready nodes run in parallel; a node that
//! fails past its retries skips everything downstream. Run state is written to
//! `<workspace>/pipelines/<id>.yml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use forge_workspace::pipeline::{self, RunConfig};
use serde_json::json;

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
fn flags(args: &mut Vec<String>, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(v) = flag(args, name) {
        out.push(v);
    }
    out
}
fn has(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        true
    } else {
        false
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("pipeline error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if !args.is_empty() && !args[0].starts_with('-') {
        args.remove(0)
    } else {
        "run".to_string()
    };
    if cmd != "run" {
        bail!("unknown command `{cmd}` — usage: forge-pipeline run <flow.yaml> [--input k=v]...");
    }

    // Positional: the workflow file.
    let file = args
        .iter()
        .position(|a| !a.starts_with("--"))
        .map(|i| args.remove(i))
        .context("missing workflow file (forge-pipeline run <flow.yaml>)")?;
    let raw = std::fs::read_to_string(&file).with_context(|| format!("read {file}"))?;
    let wf = pipeline::parse_workflow(&raw)?;

    // --input k=v (repeatable), then fill defaults for anything unset.
    let mut input: BTreeMap<String, String> = BTreeMap::new();
    for kv in flags(&mut args, "--input") {
        let (k, v) = kv.split_once('=').with_context(|| format!("--input expects k=v, got `{kv}`"))?;
        input.insert(k.to_string(), v.to_string());
    }
    for (k, d) in &wf.input_defaults {
        input.entry(k.clone()).or_insert_with(|| d.clone());
    }
    // Warn about declared inputs with no value (non-fatal).
    for k in &wf.input_keys {
        if !input.contains_key(k) {
            eprintln!("  ⚠ input `{k}` not provided (templates using it resolve empty)");
        }
    }

    let default_project = flag(&mut args, "--project")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .context("resolve --project")?;
    let workspace = flag(&mut args, "--workspace")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_project.join(".forge-workspace"));
    std::fs::create_dir_all(&workspace)?;
    let workspace = workspace.canonicalize()?;

    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
    let forge = flag(&mut args, "--forge").map(PathBuf::from).unwrap_or_else(|| exe_dir.join("forge"));
    let mcp_bin = exe_dir.join("forge-workspace-mcp");

    let isolate = has(&mut args, "--isolate-mcp");
    let home = if isolate { Some(setup_isolated_home(&workspace, &mcp_bin)?) } else { None };

    let cfg = RunConfig {
        forge,
        default_project,
        workspace,
        concurrent: flag(&mut args, "--concurrent").and_then(|s| s.parse().ok()).unwrap_or(2).max(1),
        node_timeout: Duration::from_secs(
            flag(&mut args, "--node-timeout-secs").and_then(|s| s.parse().ok()).unwrap_or(300),
        ),
        home,
        dry_run: has(&mut args, "--dry-run"),
    };

    if let Some(unknown) = args.iter().find(|a| a.starts_with("--")) {
        bail!("unknown flag `{unknown}`");
    }

    let result = pipeline::run(&wf, input, &cfg)?;
    if result.status == pipeline::PipelineStatus::Failed {
        std::process::exit(1);
    }
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Isolated base_path (FORGE_CONFIG) exposing ONLY the workspace MCP — mirrors
/// forge-workspace-run's isolate so `claude`-mode nodes start fast/reliably.
/// Credentials are symlinked, never copied.
fn setup_isolated_home(workspace: &Path, mcp_bin: &Path) -> Result<PathBuf> {
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
