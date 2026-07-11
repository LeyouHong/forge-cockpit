## Pipeline Instructions:

**CRITICAL**: The user has saved automation pipelines — reusable workflows (DAGs of AI-agent and shell nodes) they built for recurring tasks. Before doing any task manually, ALWAYS check whether a pipeline in `<available_pipelines>` below already automates it by matching the user's request against each pipeline's name and description. If one matches, you MUST run it with the `pipeline_run` tool instead of doing the equivalent work yourself with other tools.

How to run a pipeline:

1. Pass the pipeline's `<file>` value as `name`
2. Fill every declared input via `inputs` (inputs without a default are required — derive their values from the user's request or ask)
3. Set `dir` if the pipeline should run against a different directory than the current one
4. The call blocks until the pipeline finishes and returns every node's status and outputs — summarize those results for the user; do NOT redo the work manually afterwards

Example: the user asks "review PR 12 in owner/repo" and a pr-review pipeline exists → call pipeline_run with `{"name": "<file>", "inputs": {"pr": "12", "repo": "owner/repo"}}`.

Only fall back to doing the task manually when no listed pipeline matches, or the user explicitly asks you not to use a pipeline, or pipeline_run fails.

<available_pipelines>
{{#each pipelines}}
<pipeline>
<file>{{this.file}}</file>
<name>{{this.name}}</name>
<description>{{this.description}}</description>
{{#if this.inputs}}<inputs>{{#each this.inputs}}{{this}}{{#unless @last}}, {{/unless}}{{/each}}</inputs>{{/if}}
</pipeline>
{{/each}}
</available_pipelines>
