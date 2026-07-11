//! Run a YAML workflow (a DAG of AI/shell tasks) to completion, or watch a repo
//! and fire one per new/updated PR.
//!
//!   forge-pipeline run <flow.yaml> [--input k=v]... [--project DIR]
//!       [--workspace DIR] [--forge PATH] [--concurrent N]
//!       [--node-timeout-secs N] [--isolate-mcp] [--dry-run]
//!
//!   forge-pipeline watch <flow.yaml> --repo owner/name [--input-name pr]
//!       [--poll-secs N] [--once] [<same run flags>]
//!
//! Node prompts template `{{input.x}}`, `{{vars.y}}`, and
//! `{{nodes.<id>.outputs.<name>}}`; ready nodes run in parallel; a node that
//! fails past its retries skips everything downstream. Run state is written to
//! `<workspace>/pipelines/<id>.yml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use forge_workspace::pipeline::{self, RunConfig, Workflow};
use serde_json::{json, Value};

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
    match cmd.as_str() {
        "run" => do_run(args),
        "watch" => do_watch(args),
        other => bail!("unknown command `{other}` — usage: forge-pipeline run|watch <flow.yaml> ..."),
    }
}

/// Parse the workflow file + common run flags, consuming them from `args`.
fn build(args: &mut Vec<String>) -> Result<(Workflow, RunConfig)> {
    let file = args
        .iter()
        .position(|a| !a.starts_with("--"))
        .map(|i| args.remove(i))
        .context("missing workflow file (<flow.yaml>)")?;
    let raw = std::fs::read_to_string(&file).with_context(|| format!("read {file}"))?;
    let wf = pipeline::parse_workflow(&raw)?;

    let default_project = flag(args, "--project")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .context("resolve --project")?;
    let workspace = flag(args, "--workspace")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_project.join(".forge-workspace"));
    std::fs::create_dir_all(&workspace)?;
    let workspace = workspace.canonicalize()?;

    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
    let forge = flag(args, "--forge").map(PathBuf::from).unwrap_or_else(|| exe_dir.join("forge"));
    let mcp_bin = exe_dir.join("forge-workspace-mcp");
    let home = if has(args, "--isolate-mcp") {
        Some(setup_isolated_home(&workspace, &mcp_bin)?)
    } else {
        None
    };

    let cfg = RunConfig {
        forge,
        default_project,
        workspace,
        concurrent: flag(args, "--concurrent").and_then(|s| s.parse().ok()).unwrap_or(2).max(1),
        node_timeout: Duration::from_secs(
            flag(args, "--node-timeout-secs").and_then(|s| s.parse().ok()).unwrap_or(300),
        ),
        home,
        dry_run: has(args, "--dry-run"),
    };
    Ok((wf, cfg))
}

fn do_run(mut args: Vec<String>) -> Result<()> {
    // Collect --input before build() so it isn't caught by the unknown-flag check.
    let input_kvs = flags(&mut args, "--input");
    let (wf, cfg) = build(&mut args)?;
    if let Some(unknown) = args.iter().find(|a| a.starts_with("--")) {
        bail!("unknown flag `{unknown}`");
    }

    let mut input: BTreeMap<String, String> = BTreeMap::new();
    for kv in input_kvs {
        let (k, v) = kv.split_once('=').with_context(|| format!("--input expects k=v, got `{kv}`"))?;
        input.insert(k.to_string(), v.to_string());
    }
    for (k, d) in &wf.input_defaults {
        input.entry(k.clone()).or_insert_with(|| d.clone());
    }
    for k in &wf.input_keys {
        if !input.contains_key(k) {
            eprintln!("  ⚠ input `{k}` not provided (templates using it resolve empty)");
        }
    }

    let result = pipeline::run(&wf, input, &cfg)?;
    if result.status == pipeline::PipelineStatus::Failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Resident PR trigger: poll `--repo` for open PRs and fire the workflow once per
/// new PR or new head commit (deduped by `<number>@<sha>`, persisted so restarts
/// don't re-fire). The PR number is passed to the workflow as `--input-name`.
fn do_watch(mut args: Vec<String>) -> Result<()> {
    let repo = flag(&mut args, "--repo").context("watch needs --repo owner/name")?;
    let input_name = flag(&mut args, "--input-name").unwrap_or_else(|| "pr".into());
    let poll = Duration::from_secs(flag(&mut args, "--poll-secs").and_then(|s| s.parse().ok()).unwrap_or(60));
    let once = has(&mut args, "--once");
    let (wf, cfg) = build(&mut args)?;
    if let Some(unknown) = args.iter().find(|a| a.starts_with("--")) {
        bail!("unknown flag `{unknown}`");
    }

    let seen_path = cfg.workspace.join("pipelines").join(".watch.json");
    println!(
        "👁  watching {repo} — firing `{}` per new/updated PR (input `{input_name}`, poll {}s){}",
        wf.name,
        poll.as_secs(),
        if once { " [once]" } else { "" }
    );
    loop {
        match list_open_prs(&repo) {
            Ok(prs) => {
                let mut seen = load_seen(&seen_path);
                let mut fired = 0;
                for (num, sha, title) in &prs {
                    let key = format!("{num}@{sha}");
                    if seen.contains_key(&key) {
                        continue;
                    }
                    println!("\n▶ PR #{num} [{}] {title} — firing pipeline", &sha[..sha.len().min(7)]);
                    // Supply both the PR number (as `--input-name`) and the repo,
                    // so the workflow's gh commands are repo-agnostic.
                    let input = BTreeMap::from([
                        (input_name.clone(), num.to_string()),
                        ("repo".to_string(), repo.clone()),
                    ]);
                    if let Err(e) = pipeline::run(&wf, input, &cfg) {
                        eprintln!("  ! pipeline error on PR #{num}: {e:#}");
                    }
                    // Mark seen after firing (whether it succeeded) so we don't
                    // spin on a persistently-failing PR; a new commit re-fires.
                    seen.insert(key, now_iso());
                    save_seen(&seen_path, &seen);
                    fired += 1;
                }
                if fired == 0 {
                    println!("  … no new PRs ({} open, all already seen)", prs.len());
                }
            }
            Err(e) => eprintln!("  ! gh pr list failed: {e:#}"),
        }
        if once {
            break;
        }
        std::thread::sleep(poll);
    }
    Ok(())
}

/// Open PRs as (number, head sha, title) via the authenticated `gh` CLI.
fn list_open_prs(repo: &str) -> Result<Vec<(u64, String, String)>> {
    let out = std::process::Command::new("gh")
        .args([
            "pr", "list", "--repo", repo, "--state", "open",
            "--json", "number,headRefOid,title", "--limit", "50",
        ])
        .output()
        .context("run gh pr list")?;
    if !out.status.success() {
        bail!("gh pr list: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("parse gh json")?;
    let empty = Vec::new();
    let mut prs = Vec::new();
    for p in v.as_array().unwrap_or(&empty) {
        let (Some(num), Some(sha)) = (
            p.get("number").and_then(Value::as_u64),
            p.get("headRefOid").and_then(Value::as_str),
        ) else {
            continue;
        };
        let title = p.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        prs.push((num, sha.to_string(), title));
    }
    Ok(prs)
}

fn load_seen(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_seen(path: &Path, seen: &BTreeMap<String, String>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(seen).unwrap_or_default());
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
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
