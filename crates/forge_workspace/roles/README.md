# Role SOPs

Each Markdown file here is a Standard Operating Procedure that the orchestrator
(`forge-workspace-run`) injects as an agent's prompt, alongside a live
[topology](../src/bin/forge-workspace-run.rs) snapshot of the board. Agents
coordinate only through the workspace MCP tools and the shared request
documents — never directly.

| Role | File | Does |
|------|------|------|
| PM | [`pm.md`](pm.md) | writes the PRD — requirements + testable acceptance criteria for a goal |
| Architect | [`architect.md`](architect.md) | designs against the PRD, decomposes it into work requests |
| Coordinator (Lead) | [`coordinator.md`](coordinator.md) | sanity-checks the board against the goal/PRD, fills gaps |
| Engineer | [`engineer.md`](engineer.md) | claims open work, checks the inbox, implements against the acceptance criteria |
| Reviewer | [`reviewer.md`](reviewer.md) | reviews the work (criteria + quality + security + performance), verifies each finding |
| QA | [`qa.md`](qa.md) | verifies each acceptance criterion by actually running tests |

Planning flows **pm → architect → coordinator** (PRD → design + requests →
sanity pass), then work flows **engineer → reviewer → qa → done**, handed off
automatically by request status.
