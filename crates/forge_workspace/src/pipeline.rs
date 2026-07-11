//! A general workflow engine — YAML-defined DAGs of AI/shell tasks.
//!
//! A **workflow** is a directed acyclic graph of nodes. Each node runs a prompt
//! (as a `forge -p` agent) or a shell command, can depend on other nodes, and
//! exposes **outputs** that downstream nodes template into their own prompts
//! (`{{nodes.<id>.outputs.<name>}}`). Ready nodes run in parallel; a failed node
//! (after its retries) skips everything downstream of it.
//!
//! This is the Rust port of the `@aion0/forge` pipeline concept, reduced to its
//! core: parse → template → topological schedule → run → pass outputs forward.
//! Advanced features (for_each, verify/replan, reflexion, conversation mode,
//! worktree isolation, plugins) are intentionally out of scope for this layer.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ─── Workflow definition (parsed from YAML) ────────────────────────────────

/// How a node's work is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeMode {
    /// Run the prompt as a `forge -p` agent (default).
    #[default]
    Claude,
    /// Run the prompt as a shell command (`sh -c`).
    Shell,
}

/// What a named output captures from a node's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extract {
    /// The node's textual output (agent answer / command stdout), trimmed.
    #[default]
    Result,
    /// `git diff` in the node's project directory after it ran.
    GitDiff,
    /// Raw stdout, verbatim (no trimming) — for shell nodes piping data on.
    Stdout,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    pub name: String,
    #[serde(default)]
    pub extract: Extract,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowNode {
    #[serde(skip)]
    pub id: String,
    /// Project dir the node runs in (templated). Empty → the run's default.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub mode: NodeMode,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<Output>,
    /// Extra attempts on failure (0 = fail-fast).
    #[serde(default)]
    pub retries: u32,
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub vars: BTreeMap<String, String>,
    /// Declared input field names, in author order.
    pub input_keys: Vec<String>,
    /// Defaults for inputs that declared one.
    pub input_defaults: BTreeMap<String, String>,
    /// Nodes in author order (also the display order).
    pub nodes: Vec<WorkflowNode>,
}

impl Workflow {
    pub fn node(&self, id: &str) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

#[derive(Deserialize)]
struct WorkflowRaw {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    input: serde_yml::Mapping,
    nodes: serde_yml::Mapping,
}

/// Parse a workflow YAML document and validate its DAG shape.
pub fn parse_workflow(raw: &str) -> Result<Workflow> {
    let doc: WorkflowRaw = serde_yml::from_str(raw).context("parse workflow yaml")?;

    // input: values are either a plain description string or an object that may
    // carry a `default`. We only need the keys and any defaults here.
    let mut input_keys = Vec::new();
    let mut input_defaults = BTreeMap::new();
    for (key, v) in doc.input.iter() {
        input_keys.push(key.clone());
        if let Some(map) = v.as_mapping() {
            if let Some(d) = map.get("default") {
                input_defaults.insert(key.clone(), value_to_string(d));
            }
        }
    }

    let mut nodes = Vec::new();
    for (id, v) in doc.nodes.iter() {
        let mut node: WorkflowNode =
            serde_yml::from_value(v.clone()).with_context(|| format!("parse node `{id}`"))?;
        node.id = id.clone();
        nodes.push(node);
    }

    let wf = Workflow {
        name: doc.name,
        description: doc.description,
        vars: doc.vars,
        input_keys,
        input_defaults,
        nodes,
    };
    validate(&wf)?;
    Ok(wf)
}

/// Reject unknown dependencies, duplicate node ids, and dependency cycles.
fn validate(wf: &Workflow) -> Result<()> {
    if wf.nodes.is_empty() {
        bail!("workflow `{}` has no nodes", wf.name);
    }
    let ids: HashSet<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
    if ids.len() != wf.nodes.len() {
        bail!("workflow `{}` has duplicate node ids", wf.name);
    }
    for n in &wf.nodes {
        for d in &n.depends_on {
            if !ids.contains(d.as_str()) {
                bail!("node `{}` depends on unknown node `{}`", n.id, d);
            }
        }
    }
    // Cycle check: repeatedly peel nodes whose deps are all resolved.
    let mut resolved: HashSet<&str> = HashSet::new();
    loop {
        let before = resolved.len();
        for n in &wf.nodes {
            if !resolved.contains(n.id.as_str())
                && n.depends_on.iter().all(|d| resolved.contains(d.as_str()))
            {
                resolved.insert(n.id.as_str());
            }
        }
        if resolved.len() == wf.nodes.len() {
            break;
        }
        if resolved.len() == before {
            bail!("workflow `{}` has a dependency cycle", wf.name);
        }
    }
    Ok(())
}

// ─── Pipeline run state (persisted) ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeState {
    pub status: NodeStatus,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: String,
    pub workflow_name: String,
    pub status: PipelineStatus,
    pub input: BTreeMap<String, String>,
    pub vars: BTreeMap<String, String>,
    pub nodes: BTreeMap<String, NodeState>,
    /// Node ids in author order (for display).
    pub node_order: Vec<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

// ─── Template resolution ───────────────────────────────────────────────────

/// Resolve `{{…}}` placeholders against input / vars / upstream node outputs.
/// Supported: `input.<k>`, `vars.<k>`, `nodes.<id>.outputs.<name>`, and a
/// leading `raw:` (kept for compatibility; a no-op here since we pass args
/// directly rather than through a shell). Unknown expressions are left intact.
pub fn resolve_template(
    tpl: &str,
    input: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
    nodes: &BTreeMap<String, NodeState>,
) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // no closing brace — emit the rest verbatim
            out.push_str("{{");
            rest = after;
            continue;
        };
        let mut expr = after[..end].trim();
        if let Some(stripped) = expr.strip_prefix("raw:") {
            expr = stripped.trim();
        }
        out.push_str(&resolve_expr(expr, input, vars, nodes));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn resolve_expr(
    expr: &str,
    input: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
    nodes: &BTreeMap<String, NodeState>,
) -> String {
    if let Some(k) = expr.strip_prefix("input.") {
        return input.get(k).cloned().unwrap_or_default();
    }
    if let Some(k) = expr.strip_prefix("vars.") {
        return vars.get(k).cloned().unwrap_or_default();
    }
    // nodes.<id>.outputs.<name>
    if let Some(tail) = expr.strip_prefix("nodes.") {
        if let Some((node_id, out_name)) = tail.split_once(".outputs.") {
            return nodes
                .get(node_id)
                .and_then(|s| s.outputs.get(out_name))
                .cloned()
                .unwrap_or_default();
        }
    }
    // Unknown — leave the placeholder so it's visible rather than silently gone.
    format!("{{{{{expr}}}}}")
}

fn value_to_string(v: &serde_yml::Value) -> String {
    match v {
        serde_yml::Value::String(s) => s.clone(),
        serde_yml::Value::Bool(b) => b.to_string(),
        serde_yml::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

// ─── Execution ─────────────────────────────────────────────────────────────

pub struct RunConfig {
    /// The `forge` binary used for `claude`-mode nodes.
    pub forge: PathBuf,
    /// Default project dir when a node doesn't set its own.
    pub default_project: PathBuf,
    /// Where pipeline state is persisted (`<workspace>/pipelines/<id>.yml`).
    pub workspace: PathBuf,
    /// Max nodes running at once.
    pub concurrent: usize,
    /// Per-node wall-clock limit.
    pub node_timeout: Duration,
    /// Optional isolated base_path (FORGE_CONFIG) for `claude`-mode nodes.
    pub home: Option<PathBuf>,
    /// Don't spawn anything — simulate instant success (for logic tests).
    pub dry_run: bool,
}

/// Outcome of one node's execution, sent back to the scheduler.
struct Done {
    id: String,
    result: std::result::Result<BTreeMap<String, String>, String>,
}

/// Run a workflow to completion. Returns the final pipeline state.
pub fn run(wf: &Workflow, input: BTreeMap<String, String>, cfg: &RunConfig) -> Result<Pipeline> {
    let id = new_id();
    let node_order: Vec<String> = wf.nodes.iter().map(|n| n.id.clone()).collect();
    let nodes: BTreeMap<String, NodeState> = node_order
        .iter()
        .map(|id| {
            (
                id.clone(),
                NodeState { status: NodeStatus::Pending, outputs: Default::default(), attempts: 0, error: None },
            )
        })
        .collect();
    let pipeline = Arc::new(Mutex::new(Pipeline {
        id: id.clone(),
        workflow_name: wf.name.clone(),
        status: PipelineStatus::Running,
        input,
        vars: wf.vars.clone(),
        nodes,
        node_order,
        created_at: now(),
        completed_at: None,
    }));
    persist(&pipeline, cfg);

    let (tx, rx) = mpsc::channel::<Done>();
    let mut running: usize = 0;

    println!("▶ pipeline {id} — workflow `{}` ({} nodes)", wf.name, wf.nodes.len());

    loop {
        // Cascade skips from failed/skipped dependencies, then launch every
        // ready node up to the concurrency limit.
        loop {
            let launch = {
                let mut p = pipeline.lock().unwrap();
                cascade_skips(wf, &mut p);
                if running >= cfg.concurrent {
                    None
                } else {
                    ready_node(wf, &p)
                }
            };
            let Some(node_id) = launch else { break };

            // Resolve prompt + project against the current output state.
            let (mode, resolved_prompt, project, outputs, attempt) = {
                let mut p = pipeline.lock().unwrap();
                let node = wf.node(&node_id).unwrap().clone();
                let prompt = resolve_template(&node.prompt, &p.input, &p.vars, &p.nodes);
                let proj = resolve_template(&node.project, &p.input, &p.vars, &p.nodes);
                let st = p.nodes.get_mut(&node_id).unwrap();
                st.status = NodeStatus::Running;
                st.attempts += 1;
                (node.mode, prompt, proj, node.outputs, st.attempts)
            };
            persist(&pipeline, cfg);

            let project_dir = if project.trim().is_empty() {
                cfg.default_project.clone()
            } else {
                PathBuf::from(project)
            };
            println!("  → {node_id} [{mode:?}] attempt {attempt}{}", if cfg.dry_run { " (dry-run)" } else { "" });

            running += 1;
            let tx = tx.clone();
            let forge = cfg.forge.clone();
            let home = cfg.home.clone();
            let timeout = cfg.node_timeout;
            let dry = cfg.dry_run;
            std::thread::spawn(move || {
                let result = if dry {
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(outputs.iter().map(|o| (o.name.clone(), format!("<dry:{}>", o.name))).collect())
                } else {
                    exec_node(mode, &resolved_prompt, &project_dir, &outputs, &forge, home.as_deref(), timeout)
                };
                let _ = tx.send(Done { id: node_id, result });
            });
        }

        // Nothing running and nothing launchable → we're done (or deadlocked,
        // which validate() already ruled out for pure DAGs).
        if running == 0 {
            break;
        }

        // Block for the next node to finish, then fold its result in.
        let done = rx.recv().expect("scheduler channel closed");
        running -= 1;
        {
            let mut p = pipeline.lock().unwrap();
            match done.result {
                Ok(outs) => {
                    let st = p.nodes.get_mut(&done.id).unwrap();
                    st.outputs = outs;
                    st.status = NodeStatus::Done;
                    st.error = None;
                    println!("  ✓ {} done", done.id);
                }
                Err(e) => {
                    let node = wf.node(&done.id).unwrap();
                    let st = p.nodes.get_mut(&done.id).unwrap();
                    if st.attempts <= node.retries {
                        // Reset to pending for another attempt.
                        st.status = NodeStatus::Pending;
                        println!("  ↻ {} failed (attempt {}/{}) — retrying: {}", done.id, st.attempts, node.retries + 1, e);
                    } else {
                        st.status = NodeStatus::Failed;
                        st.error = Some(e.clone());
                        println!("  ✗ {} failed: {}", done.id, e);
                    }
                }
            }
        }
        persist(&pipeline, cfg);
    }

    // Settle final status.
    let mut p = pipeline.lock().unwrap();
    let any_failed = p.nodes.values().any(|n| n.status == NodeStatus::Failed);
    p.status = if any_failed { PipelineStatus::Failed } else { PipelineStatus::Done };
    p.completed_at = Some(now());
    let final_state = p.clone();
    drop(p);
    persist(&pipeline, cfg);

    println!("\n===== pipeline {} =====", final_state.status_str());
    for id in &final_state.node_order {
        let st = &final_state.nodes[id];
        println!("  {id:20} {:?}{}", st.status, st.error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default());
    }
    Ok(final_state)
}

impl Pipeline {
    fn status_str(&self) -> String {
        format!("{:?}", self.status).to_lowercase()
    }
}

/// Mark still-pending nodes as skipped when any dependency failed or was skipped.
fn cascade_skips(wf: &Workflow, p: &mut Pipeline) {
    loop {
        let mut changed = false;
        for node in &wf.nodes {
            let st = &p.nodes[&node.id];
            if st.status != NodeStatus::Pending {
                continue;
            }
            let blocked = node.depends_on.iter().any(|d| {
                matches!(p.nodes.get(d).map(|s| s.status), Some(NodeStatus::Failed) | Some(NodeStatus::Skipped))
            });
            if blocked {
                p.nodes.get_mut(&node.id).unwrap().status = NodeStatus::Skipped;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// The next pending node whose dependencies are all done (author order).
fn ready_node(wf: &Workflow, p: &Pipeline) -> Option<String> {
    wf.nodes
        .iter()
        .find(|n| {
            p.nodes[&n.id].status == NodeStatus::Pending
                && n.depends_on.iter().all(|d| p.nodes[d].status == NodeStatus::Done)
        })
        .map(|n| n.id.clone())
}

/// Run one node and extract its declared outputs.
fn exec_node(
    mode: NodeMode,
    prompt: &str,
    project: &Path,
    outputs: &[Output],
    forge: &Path,
    home: Option<&Path>,
    timeout: Duration,
) -> std::result::Result<BTreeMap<String, String>, String> {
    let raw_stdout = match mode {
        NodeMode::Claude => {
            let mut cmd = Command::new(forge);
            cmd.arg("-p").arg(prompt).current_dir(project);
            if let Some(h) = home {
                cmd.env("FORGE_CONFIG", h);
            }
            run_capture(cmd, timeout)?
        }
        NodeMode::Shell => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(prompt).current_dir(project);
            run_capture(cmd, timeout)?
        }
    };

    let mut out = BTreeMap::new();
    for o in outputs {
        let v = match o.extract {
            Extract::Result => clean_result(&raw_stdout),
            Extract::Stdout => raw_stdout.clone(),
            Extract::GitDiff => {
                let mut cmd = Command::new("git");
                cmd.arg("diff").current_dir(project);
                run_capture(cmd, timeout)?
            }
        };
        out.insert(o.name.clone(), v);
    }
    Ok(out)
}

/// Spawn a command, drain stdout on a reader thread (so a full pipe can't
/// deadlock the process), and kill it if it outlives `timeout`.
fn run_capture(mut cmd: Command, timeout: Duration) -> std::result::Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let out = reader.join().unwrap_or_default();
    match status {
        Some(s) if s.success() => Ok(out),
        Some(s) => Err(format!("exited with {}", s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()))),
        None => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// Clean a `forge -p` capture into usable `result` text: strip ANSI, then drop
/// the session chrome (`● [HH:MM:SS] Initialize/Finished …`) and spinner lines
/// (`… · Ctrl+C to interrupt`) forge streams to stdout. What remains is the
/// agent's reasoning + answer. NOTE: forge has no clean-answer output mode, so
/// this still includes the model's thinking — isolating just the final answer
/// would need a structured output from forge itself.
fn clean_result(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut out: Vec<&str> = Vec::new();
    for line in stripped.lines() {
        let t = line.trim_end();
        let tl = t.trim_start();
        if tl.is_empty() && out.last().map(|l: &&str| l.is_empty()).unwrap_or(true) {
            continue; // collapse blank runs
        }
        if tl.contains("· Ctrl+C to interrupt") {
            continue; // spinner
        }
        if tl.starts_with("● [") && (tl.contains("Initialize") || tl.contains("Finished")) {
            continue; // session chrome
        }
        out.push(t);
    }
    out.join("\n").trim().to_string()
}

/// Strip ANSI/VT escape sequences (forge -p emits heavy spinner output).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC — skip a CSI/other sequence up to its final byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn persist(pipeline: &Arc<Mutex<Pipeline>>, cfg: &RunConfig) {
    let p = pipeline.lock().unwrap();
    let dir = cfg.workspace.join("pipelines");
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(yaml) = serde_yml::to_string(&*p) {
            let _ = std::fs::write(dir.join(format!("{}.yml", p.id)), yaml);
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_id() -> String {
    let raw = forge_domain::ConversationId::generate().into_string();
    let short: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("pl-{short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn template_resolves_input_vars_and_node_outputs() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "design".to_string(),
            NodeState { status: NodeStatus::Done, outputs: map(&[("spec", "THE SPEC")]), attempts: 1, error: None },
        );
        let input = map(&[("feature", "login")]);
        let vars = map(&[("project", "app")]);
        let got = resolve_template(
            "{{vars.project}}: {{input.feature}} => {{nodes.design.outputs.spec}} [{{input.missing}}]",
            &input,
            &vars,
            &nodes,
        );
        assert_eq!(got, "app: login => THE SPEC []");
    }

    #[test]
    fn clean_result_drops_chrome_and_spinner() {
        let raw = "\u{1b}[2K⠋ Thinking 00s · Ctrl+C to interrupt\n\
                   ● [21:26:30] Initialize abc-123\n\
                   The user asked for BANANA.\n\
                   BANANA\n\
                   ● [21:26:33] Finished abc-123\n";
        assert_eq!(clean_result(raw), "The user asked for BANANA.\nBANANA");
    }

    #[test]
    fn unknown_placeholder_is_left_intact() {
        let e = BTreeMap::new();
        let nodes = BTreeMap::new();
        assert_eq!(resolve_template("{{bogus.path}}", &e, &e, &nodes), "{{bogus.path}}");
    }

    #[test]
    fn parse_and_validate_dag() {
        let yaml = r#"
name: demo
vars: { project: app }
input: { feature: "what to build" }
nodes:
  design:
    prompt: "design {{input.feature}}"
    outputs: [{ name: spec, extract: result }]
  build:
    depends_on: [design]
    prompt: "build {{nodes.design.outputs.spec}}"
"#;
        let wf = parse_workflow(yaml).unwrap();
        assert_eq!(wf.name, "demo");
        assert_eq!(wf.nodes.len(), 2);
        assert_eq!(wf.nodes[0].id, "design"); // author order preserved
        assert_eq!(wf.node("build").unwrap().depends_on, vec!["design"]);
    }

    #[test]
    fn cycle_is_rejected() {
        let yaml = r#"
name: loop
input: {}
nodes:
  a: { prompt: x, depends_on: [b] }
  b: { prompt: y, depends_on: [a] }
"#;
        assert!(parse_workflow(yaml).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let yaml = r#"
name: bad
input: {}
nodes:
  a: { prompt: x, depends_on: [ghost] }
"#;
        assert!(parse_workflow(yaml).unwrap_err().to_string().contains("unknown node"));
    }
}
