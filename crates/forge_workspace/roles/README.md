# Role SOPs

Each Markdown file here is a Standard Operating Procedure that the orchestrator
(`forge-workspace-run`) injects as an agent's prompt, alongside a live
[topology](../src/bin/forge-workspace-run.rs) snapshot of the board. Agents
coordinate only through the workspace MCP tools and the shared request
documents — never directly.

| Role | File | Does |
|------|------|------|
| Coordinator (Lead) | [`coordinator.md`](coordinator.md) | turns a goal into focused, testable work requests |
| Engineer | [`engineer.md`](engineer.md) | claims open work, checks the inbox, implements against the acceptance criteria |
| Reviewer | [`reviewer.md`](reviewer.md) | reviews the work (criteria + quality + security + performance), verifies each finding |
| QA | [`qa.md`](qa.md) | verifies each acceptance criterion by actually running tests |

Work flows **coordinator → engineer → reviewer → qa → done**, handed off
automatically by request status.
