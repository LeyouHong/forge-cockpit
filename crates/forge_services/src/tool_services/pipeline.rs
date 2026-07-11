use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use forge_app::{
    PipelineInfo, PipelineListOutput, PipelineNodeResult, PipelineRunOutput, PipelineService,
};
use forge_workspace::pipeline as engine;

/// Lists and runs the user's global pipeline recipes (built in the web UI or
/// dropped into the global pipelines dir) for the `pipeline_list` /
/// `pipeline_run` tools. The DAG engine is synchronous, so runs execute on a
/// blocking thread; `claude`-mode nodes spawn the current `forge` binary
/// against an isolated FORGE_CONFIG home.
#[derive(Default)]
pub struct ForgePipelineService {
    /// Test-only override of the `~/.forge-web` base (keeps tests off the
    /// user's real pipelines).
    base: Option<PathBuf>,
}

impl ForgePipelineService {
    pub fn new() -> Self {
        Self::default()
    }

    fn pipelines_dir(&self) -> PathBuf {
        self.base
            .as_ref()
            .map(|b| b.join("pipelines"))
            .unwrap_or_else(engine::global_pipelines_dir)
    }

    fn runs_workspace(&self) -> PathBuf {
        self.base
            .as_ref()
            .map(|b| b.join("runs"))
            .unwrap_or_else(engine::global_runs_workspace)
    }
}

#[async_trait::async_trait]
impl PipelineService for ForgePipelineService {
    async fn list_pipelines(&self) -> anyhow::Result<PipelineListOutput> {
        let dir = self.pipelines_dir();
        tokio::task::spawn_blocking(move || {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            matches!(
                                p.extension().and_then(|x| x.to_str()),
                                Some("yaml") | Some("yml")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            files.sort();

            let mut pipelines = Vec::new();
            for path in files {
                let Some(file) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                    continue;
                };
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                match engine::parse_workflow(&raw) {
                    Ok(wf) => pipelines.push(PipelineInfo {
                        file,
                        name: wf.name,
                        description: wf.description,
                        inputs: wf
                            .input_keys
                            .iter()
                            .map(|k| (k.clone(), wf.input_defaults.get(k).cloned()))
                            .collect(),
                        nodes: wf.nodes.iter().map(|n| n.id.clone()).collect(),
                    }),
                    // Surface broken files instead of hiding them: the agent can
                    // tell the user the pipeline exists but doesn't parse.
                    Err(e) => pipelines.push(PipelineInfo {
                        file,
                        name: String::new(),
                        description: format!("INVALID — does not parse: {e:#}"),
                        inputs: Vec::new(),
                        nodes: Vec::new(),
                    }),
                }
            }
            Ok(PipelineListOutput { dir, pipelines })
        })
        .await?
    }

    async fn run_pipeline(
        &self,
        name: String,
        dir: PathBuf,
        inputs: BTreeMap<String, String>,
        node_timeout: Duration,
    ) -> anyhow::Result<PipelineRunOutput> {
        let pipelines_dir = self.pipelines_dir();
        let ws = self.runs_workspace();
        tokio::task::spawn_blocking(move || {
            if name.contains('/') || name.contains("..") {
                bail!("invalid pipeline name `{name}` — use a file name from pipeline_list");
            }
            let file = pipelines_dir.join(&name);
            let raw = std::fs::read_to_string(&file).with_context(|| {
                format!("no such pipeline `{name}` — call pipeline_list for available files")
            })?;
            let wf = engine::parse_workflow(&raw)?;
            let dir = dir
                .canonicalize()
                .with_context(|| format!("target directory does not exist: {}", dir.display()))?;

            std::fs::create_dir_all(&ws)?;
            let exe = std::env::current_exe().context("locate the forge binary")?;
            let mcp_bin = exe
                .parent()
                .map(|d| d.join("forge-workspace-mcp"))
                .unwrap_or_else(|| PathBuf::from("forge-workspace-mcp"));
            let home = engine::setup_isolated_home(&ws, &mcp_bin).ok();

            let mut input = inputs;
            for (k, d) in &wf.input_defaults {
                input.entry(k.clone()).or_insert_with(|| d.clone());
            }

            let cfg = engine::RunConfig {
                forge: exe,
                default_project: dir.clone(),
                workspace: ws,
                concurrent: 2,
                node_timeout,
                home,
                dry_run: false,
            };
            let result = engine::run(&wf, input, &cfg)?;

            let status = match result.status {
                engine::PipelineStatus::Done => "done",
                engine::PipelineStatus::Failed => "failed",
                engine::PipelineStatus::Running => "running",
            };
            let nodes = result
                .node_order
                .iter()
                .filter_map(|id| result.nodes.get(id).map(|n| (id, n)))
                .map(|(id, n)| PipelineNodeResult {
                    id: id.clone(),
                    status: match n.status {
                        engine::NodeStatus::Pending => "pending",
                        engine::NodeStatus::Running => "running",
                        engine::NodeStatus::Done => "done",
                        engine::NodeStatus::Failed => "failed",
                        engine::NodeStatus::Skipped => "skipped",
                    }
                    .to_string(),
                    outputs: n.outputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    error: n.error.clone(),
                })
                .collect();

            Ok(PipelineRunOutput {
                id: result.id,
                workflow: result.workflow_name,
                status: status.to_string(),
                dir,
                nodes,
            })
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_and_run_shell_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = ForgePipelineService { base: Some(tmp.path().to_path_buf()) };

        let target = tmp.path().join("target-project");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(svc.pipelines_dir()).unwrap();
        std::fs::write(
            svc.pipelines_dir().join("smoke.yaml"),
            "name: smoke\ninput:\n  greeting: {default: hi}\nnodes:\n  say:\n    mode: shell\n    prompt: echo {{input.greeting}}\n    outputs:\n      - {name: msg, extract: stdout}\n",
        )
        .unwrap();

        let listed = svc.list_pipelines().await.unwrap();
        assert_eq!(listed.pipelines.len(), 1);
        assert_eq!(listed.pipelines[0].file, "smoke.yaml");
        assert_eq!(
            listed.pipelines[0].inputs,
            vec![("greeting".to_string(), Some("hi".to_string()))]
        );

        let mut inputs = BTreeMap::new();
        inputs.insert("greeting".to_string(), "HELLO".to_string());
        let run = svc
            .run_pipeline("smoke.yaml".to_string(), target, inputs, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(run.status, "done");
        assert_eq!(run.nodes.len(), 1);
        assert_eq!(run.nodes[0].status, "done");
        assert_eq!(run.nodes[0].outputs[0].1.trim(), "HELLO");
    }

    #[tokio::test]
    async fn test_run_rejects_traversal_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = ForgePipelineService { base: Some(tmp.path().to_path_buf()) };
        let dir = tmp.path().to_path_buf();

        let err = svc
            .run_pipeline("../evil.yaml".into(), dir.clone(), BTreeMap::new(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid pipeline name"));

        let err = svc
            .run_pipeline("nope.yaml".into(), dir, BTreeMap::new(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such pipeline"));
    }
}
