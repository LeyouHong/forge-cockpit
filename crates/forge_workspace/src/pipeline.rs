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
use std::io::{Read, Write};
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
    /// Postcondition check: on failure, instead of failing the pipeline, re-run
    /// `replan_target` and everything downstream of it, up to `max_replan` times.
    #[serde(default)]
    pub verify: bool,
    /// Node to re-run when this verify node fails (required if `verify`).
    #[serde(default)]
    pub replan_target: Option<String>,
    /// Re-plan budget (default 1: verify → redo once → accept failure).
    #[serde(default = "one")]
    pub max_replan: u32,
}

fn one() -> u32 {
    1
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
    /// When set, run the whole DAG once per item in the source list.
    pub for_each: Option<ForEach>,
}

/// Batch spec: run the whole DAG once per item, sequentially. Each iteration
/// resets per-node state; node prompts see the current item via `{{<as>}}` and
/// position via `{{loop.index}}` / `{{loop.total}}`. A `before` (aka `setup`)
/// list runs once up front and its outputs feed a dynamic `source`. (Non-goals,
/// matching the reference: nested loops, parallel iterations.)
#[derive(Debug, Clone)]
pub struct ForEach {
    pub source: Source,
    /// Separator when `source` resolves to a string. Default ",".
    pub split: String,
    /// Variable name the current item is exposed as. Default "item".
    pub as_name: String,
    pub on_failure: OnFailure,
    /// Nodes that run ONCE before the loop (a setup phase): they stay `done`
    /// across all iterations and are excluded from the per-iteration reset. The
    /// `source` is resolved AFTER they run, so it may reference their outputs.
    pub before: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Source {
    /// A templated string (`"{{input.pr_ids}}"`), split into items.
    Template(String),
    /// A literal YAML list.
    Items(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Later iterations still run; the pipeline ends `failed` if any iter failed.
    Continue,
    /// The first failing iteration halts the whole run.
    Stop,
}

/// The current-item context threaded into template resolution during a for_each.
pub struct EachCtx<'a> {
    pub as_name: &'a str,
    pub item: &'a str,
    pub index: usize,
    pub total: usize,
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
    #[serde(default)]
    for_each: Option<serde_yml::Value>,
}

fn parse_for_each(v: &serde_yml::Value) -> Result<ForEach> {
    let m = v.as_mapping().context("for_each must be a mapping")?;
    let source = match m.get("source") {
        Some(serde_yml::Value::Sequence(seq)) => Source::Items(seq.iter().map(value_to_string).collect()),
        Some(serde_yml::Value::String(s)) => Source::Template(s.clone()),
        Some(other) => Source::Template(value_to_string(other)),
        None => bail!("for_each needs a `source` (a list or a templated string)"),
    };
    let split = m.get("split").and_then(|x| x.as_str()).unwrap_or(",").to_string();
    let as_name = m.get("as").and_then(|x| x.as_str()).unwrap_or("item").to_string();
    let on_failure = match m.get("on_failure").and_then(|x| x.as_str()) {
        Some("stop") => OnFailure::Stop,
        _ => OnFailure::Continue,
    };
    let before = m
        .get("before")
        .or_else(|| m.get("setup"))
        .and_then(|v| v.as_sequence())
        .map(|s| s.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Ok(ForEach { source, split, as_name, on_failure, before })
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

    let for_each = doc.for_each.as_ref().map(parse_for_each).transpose()?;
    let wf = Workflow {
        name: doc.name,
        description: doc.description,
        vars: doc.vars,
        input_keys,
        input_defaults,
        nodes,
        for_each,
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
        if n.verify {
            let Some(target) = n.replan_target.as_deref() else {
                bail!("verify node `{}` needs a `replan_target`", n.id);
            };
            if !ids.contains(target) {
                bail!("verify node `{}` replan_target `{}` does not exist", n.id, target);
            }
            if !downstream_of(wf, target).contains(n.id.as_str()) {
                bail!("verify node `{}` must be downstream of its replan_target `{}`", n.id, target);
            }
        }
    }
    if let Some(fe) = &wf.for_each {
        for b in &fe.before {
            if !ids.contains(b.as_str()) {
                bail!("for_each setup/before names unknown node `{}`", b);
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
    /// How many times this verify node has triggered a re-plan. Survives the
    /// subtree reset (so the budget is not lost when the node is re-run).
    #[serde(default)]
    pub replan_count: u32,
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
    /// One entry per for_each iteration (empty for a plain single run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iterations: Vec<IterationSummary>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationSummary {
    pub index: usize,
    pub item: String,
    /// "done" | "failed"
    pub status: String,
}

// ─── Template resolution ───────────────────────────────────────────────────

/// Resolve `{{…}}` placeholders against input / vars / upstream node outputs.
/// Supported: `input.<k>`, `vars.<k>`, `nodes.<id>.outputs.<name>`, plus the
/// for_each vars (`{{<as>}}`, `{{loop.index}}`, `{{loop.total}}`).
///
/// In `shell` mode each substituted value is POSIX single-quoted so arbitrary
/// content (quotes, spaces, newlines) lands as ONE safe argument — do NOT wrap
/// placeholders in your own quotes. A leading `raw:` opts a value out of that
/// escaping (substituted verbatim). Non-shell (agent-prompt) mode substitutes
/// verbatim. Unknown expressions are left intact.
pub fn resolve_template(
    tpl: &str,
    input: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
    nodes: &BTreeMap<String, NodeState>,
    each: Option<&EachCtx>,
    shell: bool,
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
        let mut raw = false;
        if let Some(stripped) = expr.strip_prefix("raw:") {
            raw = true;
            expr = stripped.trim();
        }
        let value = resolve_expr(expr, input, vars, nodes, each);
        if shell && !raw {
            out.push_str(&shell_quote(&value));
        } else {
            out.push_str(&value);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// POSIX single-quote a value so it survives the shell as one literal argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn resolve_expr(
    expr: &str,
    input: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
    nodes: &BTreeMap<String, NodeState>,
    each: Option<&EachCtx>,
) -> String {
    // for_each: {{<as>}}, {{loop.index}}, {{loop.total}}
    if let Some(e) = each {
        if expr == e.as_name {
            return e.item.to_string();
        }
        if expr == "loop.index" {
            return e.index.to_string();
        }
        if expr == "loop.total" {
            return e.total.to_string();
        }
    }
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
                NodeState { status: NodeStatus::Pending, outputs: Default::default(), attempts: 0, replan_count: 0, error: None },
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
        iterations: Vec::new(),
        created_at: now(),
        completed_at: None,
    }));
    persist(&pipeline, cfg);

    match &wf.for_each {
        None => {
            println!("▶ pipeline {id} — workflow `{}` ({} nodes)", wf.name, wf.nodes.len());
            let failed = run_dag(wf, &pipeline, cfg, None, &id, None);
            finalize(&pipeline, cfg, failed);
        }
        Some(fe) => {
            // Setup phase: run the `before` nodes (and their deps) once; they
            // stay done across every iteration and let the source be dynamic.
            let setup = ancestor_closure(wf, &fe.before);
            if !setup.is_empty() {
                println!("▶ pipeline {id} — workflow `{}` · setup {:?}", wf.name, fe.before);
                let setup_failed = run_dag(wf, &pipeline, cfg, None, &id, Some(&setup));
                if setup_failed {
                    println!("  ✗ setup failed — aborting before the loop.");
                    finalize(&pipeline, cfg, true);
                    let final_state = pipeline.lock().unwrap().clone();
                    return Ok(final_state);
                }
            }
            let (input, vars, nodes) = {
                let p = pipeline.lock().unwrap();
                (p.input.clone(), p.vars.clone(), p.nodes.clone())
            };
            let items = resolve_items(fe, &input, &vars, &nodes);
            let total = items.len();
            println!("▶ pipeline {id} — workflow `{}` (for_each `{}`: {total} iteration(s))", wf.name, fe.as_name);
            let mut any_failed = false;
            for (index, item) in items.iter().enumerate() {
                reset_nodes_except(&pipeline, &setup);
                println!("\n  ⟳ iteration {}/{total} — {} = {item}", index + 1, fe.as_name);
                let each = EachCtx { as_name: &fe.as_name, item, index, total };
                let failed = run_dag(wf, &pipeline, cfg, Some(&each), &id, None);
                {
                    let mut p = pipeline.lock().unwrap();
                    p.iterations.push(IterationSummary {
                        index,
                        item: item.clone(),
                        status: if failed { "failed" } else { "done" }.to_string(),
                    });
                }
                persist(&pipeline, cfg);
                any_failed |= failed;
                if failed && fe.on_failure == OnFailure::Stop {
                    println!("  ✗ iteration failed and on_failure=stop — halting.");
                    break;
                }
            }
            finalize(&pipeline, cfg, any_failed);
        }
    }

    let final_state = pipeline.lock().unwrap().clone();
    println!("\n===== pipeline {} =====", final_state.status_str());
    for it in &final_state.iterations {
        println!("  iteration {} [{}]  {}", it.index + 1, it.status, it.item);
    }
    for nid in &final_state.node_order {
        let st = &final_state.nodes[nid];
        println!("  {nid:20} {:?}{}", st.status, st.error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default());
    }
    Ok(final_state)
}

/// Run the DAG to a fixed point once (parallel, output-passing, retries, skip
/// cascade). Returns whether any node ended failed. Assumes nodes start pending.
fn run_dag(wf: &Workflow, pipeline: &Arc<Mutex<Pipeline>>, cfg: &RunConfig, each: Option<&EachCtx>, id: &str, only: Option<&HashSet<String>>) -> bool {
    let (tx, rx) = mpsc::channel::<Done>();
    let mut running: usize = 0;

    loop {
        // Cascade skips from failed/skipped dependencies, then launch every
        // ready node up to the concurrency limit.
        loop {
            let launch = {
                let mut p = pipeline.lock().unwrap();
                if only.is_none() {
                    cascade_skips(wf, &mut p);
                }
                if running >= cfg.concurrent {
                    None
                } else {
                    ready_node(wf, &p, only)
                }
            };
            let Some(node_id) = launch else { break };

            // Resolve prompt + project against the current output state.
            let (mode, resolved_prompt, project, outputs, attempt) = {
                let mut p = pipeline.lock().unwrap();
                let node = wf.node(&node_id).unwrap().clone();
                let prompt = resolve_template(&node.prompt, &p.input, &p.vars, &p.nodes, each, node.mode == NodeMode::Shell);
                let proj = resolve_template(&node.project, &p.input, &p.vars, &p.nodes, each, false);
                let st = p.nodes.get_mut(&node_id).unwrap();
                st.status = NodeStatus::Running;
                st.attempts += 1;
                (node.mode, prompt, proj, node.outputs, st.attempts)
            };
            persist(pipeline, cfg);

            let project_dir = if project.trim().is_empty() {
                cfg.default_project.clone()
            } else {
                PathBuf::from(project)
            };
            println!("  → {node_id} [{mode:?}] attempt {attempt}{}", if cfg.dry_run { " (dry-run)" } else { "" });

            // For a `claude` node with a `result` output, provision an output
            // contract file (per iteration): the agent writes its final answer
            // there and we read it back — reliable despite forge -p's mixed stdout.
            let result_file = if !cfg.dry_run
                && mode == NodeMode::Claude
                && outputs.iter().any(|o| o.extract == Extract::Result)
            {
                let dir = cfg.workspace.join("pipelines").join(".out").join(id);
                let _ = std::fs::create_dir_all(&dir);
                let prefix = each.map(|e| format!("{}-", e.index)).unwrap_or_default();
                Some(dir.join(format!("{prefix}{node_id}.txt")))
            } else {
                None
            };
            // Live streamed stdout for this node's run (overwritten each attempt/iter).
            let log_file = if cfg.dry_run {
                None
            } else {
                Some(cfg.workspace.join("pipelines").join(".log").join(id).join(format!("{node_id}.txt")))
            };

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
                    exec_node(mode, &resolved_prompt, &project_dir, &outputs, &forge, home.as_deref(), timeout, result_file.as_deref(), log_file.as_deref())
                };
                let _ = tx.send(Done { id: node_id, result });
            });
        }

        // Nothing running and nothing launchable → this pass is settled.
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
                    let node = wf.node(&done.id).unwrap().clone();
                    if node.verify {
                        // Postcondition failed: re-plan from the target instead of
                        // failing, until the budget is spent.
                        let count = p.nodes[&done.id].replan_count;
                        if count < node.max_replan {
                            let target = node.replan_target.clone().unwrap();
                            p.nodes.get_mut(&done.id).unwrap().replan_count = count + 1;
                            reset_subtree(wf, &mut p, &target);
                            println!("  ↺ verify {} failed → re-plan {}/{} from `{}`: {}", done.id, count + 1, node.max_replan, target, e);
                        } else {
                            let st = p.nodes.get_mut(&done.id).unwrap();
                            st.status = NodeStatus::Failed;
                            st.error = Some(format!("verify failed after {} re-plan(s): {e}", node.max_replan));
                            println!("  ✗ verify {} failed after {} re-plan(s): {}", done.id, node.max_replan, e);
                        }
                    } else {
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
        }
        persist(pipeline, cfg);
    }

    pipeline.lock().unwrap().nodes.values().any(|n| n.status == NodeStatus::Failed)
}

/// Resolve a for_each source into concrete items. `nodes` carries the setup
/// phase's outputs so a `{{nodes.<setup>.outputs.<x>}}` source can resolve.
fn resolve_items(
    fe: &ForEach,
    input: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
    nodes: &BTreeMap<String, NodeState>,
) -> Vec<String> {
    match &fe.source {
        Source::Items(v) => v.clone(),
        Source::Template(t) => {
            let resolved = resolve_template(t, input, vars, nodes, None, false);
            resolved.split(&fe.split).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        }
    }
}

/// Settle the pipeline's terminal status and persist.
fn finalize(pipeline: &Arc<Mutex<Pipeline>>, cfg: &RunConfig, any_failed: bool) {
    {
        let mut p = pipeline.lock().unwrap();
        p.status = if any_failed { PipelineStatus::Failed } else { PipelineStatus::Done };
        p.completed_at = Some(now());
    }
    persist(pipeline, cfg);
}

impl Pipeline {
    fn status_str(&self) -> String {
        format!("{:?}", self.status).to_lowercase()
    }
}

/// Every node that transitively depends on `root` (excludes `root` itself).
fn downstream_of<'a>(wf: &'a Workflow, root: &str) -> HashSet<&'a str> {
    let mut set: HashSet<&str> = HashSet::new();
    loop {
        let before = set.len();
        for n in &wf.nodes {
            if set.contains(n.id.as_str()) {
                continue;
            }
            if n.depends_on.iter().any(|d| d == root || set.contains(d.as_str())) {
                set.insert(n.id.as_str());
            }
        }
        if set.len() == before {
            break;
        }
    }
    set
}

/// Reset `root` and everything downstream of it to pending for a re-plan pass.
/// `replan_count` is deliberately preserved so the budget survives the reset.
fn reset_subtree(wf: &Workflow, p: &mut Pipeline, root: &str) {
    let mut set = downstream_of(wf, root);
    set.insert(root);
    for id in set {
        if let Some(st) = p.nodes.get_mut(id) {
            st.status = NodeStatus::Pending;
            st.outputs.clear();
            st.attempts = 0;
            st.error = None;
        }
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
/// When `only` is set, restrict scheduling to that node set (used by the
/// for_each setup phase to run just the `before` nodes).
fn ready_node(wf: &Workflow, p: &Pipeline, only: Option<&HashSet<String>>) -> Option<String> {
    wf.nodes
        .iter()
        .find(|n| {
            only.map_or(true, |s| s.contains(&n.id))
                && p.nodes[&n.id].status == NodeStatus::Pending
                && n.depends_on.iter().all(|d| p.nodes[d].status == NodeStatus::Done)
        })
        .map(|n| n.id.clone())
}

/// A node set plus all their transitive dependencies (for the setup closure).
fn ancestor_closure(wf: &Workflow, roots: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = roots.iter().cloned().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for id in set.iter().cloned().collect::<Vec<_>>() {
            if let Some(n) = wf.node(&id) {
                for d in &n.depends_on {
                    if set.insert(d.clone()) {
                        changed = true;
                    }
                }
            }
        }
    }
    set
}

/// Reset every node NOT in `keep` to pending (used between for_each iterations
/// to preserve the setup phase's results).
fn reset_nodes_except(pipeline: &Arc<Mutex<Pipeline>>, keep: &HashSet<String>) {
    let mut p = pipeline.lock().unwrap();
    for (id, st) in p.nodes.iter_mut() {
        if keep.contains(id) {
            continue;
        }
        st.status = NodeStatus::Pending;
        st.outputs.clear();
        st.attempts = 0;
        st.error = None;
    }
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
    result_file: Option<&Path>,
    log_file: Option<&Path>,
) -> std::result::Result<BTreeMap<String, String>, String> {
    let log = log_file.map(|p| p.to_path_buf());
    let raw_stdout = match mode {
        NodeMode::Claude => {
            // Append the output contract when we're capturing a `result`, and
            // clear any stale file so a fallback can't read a previous run.
            let full_prompt = match result_file {
                Some(rf) => {
                    let _ = std::fs::remove_file(rf);
                    format!(
                        "{prompt}\n\n---\nPIPELINE OUTPUT CONTRACT: as your final action, create a \
                         NEW file at `{}` containing ONLY your final result — raw text, no \
                         commentary, no markdown fences. The file does not exist yet, so create it \
                         fresh (do NOT use overwrite mode). A later pipeline step reads that file \
                         as your output.",
                        rf.display()
                    )
                }
                None => prompt.to_string(),
            };
            let mut cmd = Command::new(forge);
            cmd.arg("-p").arg(&full_prompt).current_dir(project);
            if let Some(h) = home {
                cmd.env("FORGE_CONFIG", h);
            }
            run_capture(cmd, timeout, log)?
        }
        NodeMode::Shell => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(prompt).current_dir(project);
            run_capture(cmd, timeout, log)?
        }
    };

    let mut out = BTreeMap::new();
    for o in outputs {
        let v = match o.extract {
            // Prefer the contract file (clean agent answer); fall back to the
            // best-effort cleaned stdout if the agent didn't write it.
            Extract::Result => result_file
                .and_then(|rf| std::fs::read_to_string(rf).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| clean_result(&raw_stdout)),
            Extract::Stdout => raw_stdout.clone(),
            Extract::GitDiff => {
                let mut cmd = Command::new("git");
                cmd.arg("diff").current_dir(project);
                run_capture(cmd, timeout, None)?
            }
        };
        out.insert(o.name.clone(), v);
    }
    Ok(out)
}

/// Spawn a command, drain stdout on a reader thread (so a full pipe can't
/// deadlock the process), and kill it if it outlives `timeout`. When `log` is
/// set, stdout is tee'd to that file as it streams (for a live node log).
fn run_capture(mut cmd: Command, timeout: Duration, log: Option<PathBuf>) -> std::result::Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut logf = log.and_then(|p| {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            std::fs::File::create(&p).ok()
        });
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(f) = &mut logf {
                        let _ = f.write_all(&chunk[..n]);
                        let _ = f.flush();
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
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

/// The user's home directory (`$HOME` / `%USERPROFILE%`).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Where global (project-independent) pipeline recipes live. Shared by the web
/// UI's visual builder, the `forge-pipeline` CLI, and the agent's
/// `pipeline_*` tools so they all see the same set.
pub fn global_pipelines_dir() -> PathBuf {
    home_dir().join(".forge-web").join("pipelines")
}

/// The workspace all global pipeline runs persist to.
pub fn global_runs_workspace() -> PathBuf {
    home_dir().join(".forge-web").join("runs")
}

/// Isolated base_path (FORGE_CONFIG) exposing ONLY the workspace MCP — so
/// `claude`-mode nodes start fast/reliably regardless of the user's MCP setup.
/// Credentials are symlinked, never copied.
pub fn setup_isolated_home(workspace: &Path, mcp_bin: &Path) -> Result<PathBuf> {
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
    let mcp = serde_json::json!({ "mcpServers": { "forge-workspace": {
        "command": mcp_bin.to_string_lossy(),
        "env": { "FORGE_WORKSPACE_DIR": workspace.to_string_lossy() }
    }}});
    std::fs::write(home.join(".mcp.json"), serde_json::to_string_pretty(&mcp)?)?;
    Ok(home)
}

/// List persisted pipeline runs under `<workspace>/pipelines/`, newest first.
/// A pure read for dashboards / monitoring.
pub fn list_pipelines(workspace: &Path) -> Vec<Pipeline> {
    let dir = workspace.join("pipelines");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("yml") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(pl) = serde_yml::from_str::<Pipeline>(&text) {
                out.push(pl);
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
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
            NodeState { status: NodeStatus::Done, outputs: map(&[("spec", "THE SPEC")]), attempts: 1, replan_count: 0, error: None },
        );
        let input = map(&[("feature", "login")]);
        let vars = map(&[("project", "app")]);
        let got = resolve_template(
            "{{vars.project}}: {{input.feature}} => {{nodes.design.outputs.spec}} [{{input.missing}}]",
            &input,
            &vars,
            &nodes,
            None,
            false,
        );
        assert_eq!(got, "app: login => THE SPEC []");
    }

    #[test]
    fn shell_mode_quotes_values_safely() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "review".to_string(),
            NodeState {
                status: NodeStatus::Done,
                outputs: map(&[("f", "it's a \"bug\"\nline2")]),
                attempts: 1,
                replan_count: 0,
                error: None,
            },
        );
        let e = BTreeMap::new();
        // shell mode: value is single-quoted, inner ' escaped as '\'' — one safe arg.
        let got = resolve_template("gh pr comment --body {{nodes.review.outputs.f}}", &e, &e, &nodes, None, true);
        assert_eq!(got, "gh pr comment --body 'it'\\''s a \"bug\"\nline2'");
        // raw: opts out of escaping
        let raw = resolve_template("{{raw:nodes.review.outputs.f}}", &e, &e, &nodes, None, true);
        assert_eq!(raw, "it's a \"bug\"\nline2");
        // non-shell mode substitutes verbatim
        let plain = resolve_template("{{nodes.review.outputs.f}}", &e, &e, &nodes, None, false);
        assert_eq!(plain, "it's a \"bug\"\nline2");
    }

    #[test]
    fn template_resolves_foreach_item_and_loop() {
        let e = BTreeMap::new();
        let nodes = BTreeMap::new();
        let each = EachCtx { as_name: "pr", item: "42", index: 1, total: 3 };
        let got = resolve_template(
            "review PR {{pr}} ({{loop.index}}/{{loop.total}})",
            &e,
            &e,
            &nodes,
            Some(&each),
            false,
        );
        assert_eq!(got, "review PR 42 (1/3)");
    }

    #[test]
    fn foreach_source_string_splits_into_items() {
        let yaml = r#"
name: batch
input: { ids: "1, 2 ,3" }
for_each: { source: "{{input.ids}}", as: id }
nodes:
  work: { mode: shell, prompt: "echo {{id}}" }
"#;
        let wf = parse_workflow(yaml).unwrap();
        let fe = wf.for_each.as_ref().unwrap();
        assert_eq!(fe.as_name, "id");
        let input: BTreeMap<String, String> = [("ids".to_string(), "1, 2 ,3".to_string())].into();
        let items = resolve_items(fe, &input, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(items, vec!["1", "2", "3"]); // split + trimmed
    }

    #[test]
    fn foreach_before_setup_parsed_and_validated() {
        let yaml = r#"
name: s
input: {}
for_each: { before: [disc], source: "{{nodes.disc.outputs.l}}" }
nodes:
  disc: { mode: shell, prompt: "echo x", outputs: [{ name: l, extract: stdout }] }
  w: { depends_on: [disc], mode: shell, prompt: "echo {{item}}" }
"#;
        let wf = parse_workflow(yaml).unwrap();
        assert_eq!(wf.for_each.as_ref().unwrap().before, vec!["disc"]);
        // unknown before node is rejected
        let bad = r#"
name: s
input: {}
for_each: { before: [ghost], source: "x" }
nodes:
  a: { mode: shell, prompt: "true" }
"#;
        assert!(parse_workflow(bad).unwrap_err().to_string().contains("unknown node"));
    }

    #[test]
    fn foreach_source_literal_list() {
        let yaml = r#"
name: batch
input: {}
for_each: { source: [alpha, beta] }
nodes:
  work: { mode: shell, prompt: "echo {{item}}" }
"#;
        let wf = parse_workflow(yaml).unwrap();
        let fe = wf.for_each.as_ref().unwrap();
        let items = resolve_items(fe, &BTreeMap::new(), &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(items, vec!["alpha", "beta"]);
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
        assert_eq!(resolve_template("{{bogus.path}}", &e, &e, &nodes, None, false), "{{bogus.path}}");
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
    fn verify_requires_replan_target() {
        let yaml = r#"
name: v
input: {}
nodes:
  a: { mode: shell, prompt: "true" }
  v: { depends_on: [a], mode: shell, verify: true, prompt: "false" }
"#;
        assert!(parse_workflow(yaml).unwrap_err().to_string().contains("replan_target"));
    }

    #[test]
    fn verify_must_be_downstream_of_target() {
        let yaml = r#"
name: v
input: {}
nodes:
  a: { mode: shell, prompt: "true" }
  b: { mode: shell, prompt: "true" }
  v: { depends_on: [a], mode: shell, verify: true, replan_target: b, prompt: "false" }
"#;
        assert!(parse_workflow(yaml).unwrap_err().to_string().contains("downstream"));
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
