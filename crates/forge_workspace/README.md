# forge_workspace — a resident multi-agent team for Forge

`forge_workspace` turns Forge into a **standing team of agents** that pick up
work, hand it down a pipeline, and drive it to done — without you babysitting a
single session. It is an *outer orchestration layer*: it drives `forge -p`
subprocesses as role agents (engineer, reviewer, QA, lead) around a shared
blackboard. **It does not modify any Forge core crate**, so it stays cheap to
carry across upstream merges.

```
              ┌──────────── the blackboard (YAML on disk) ────────────┐
   goal ──▶ coordinator ──▶ request ──▶ engineer ──▶ reviewer ──▶ qa ──▶ done
   (Lead decomposes)         (open)    (in_progress)  (review)   (qa)
                                 ▲__________________________|  changes_requested
                                            rework (message bus)
```

Agents never talk to each other directly. They coordinate only through two
primitives:

- **Request documents** — the shared tasks work lives on, plus a **status state
  machine** that hands each request from role to role.
- **A message bus** — for the exceptions the state machine doesn't cover: rework
  details, escalations, stuck alerts.

---

## The state machine

```
          claim / submit_engineer_work        submit_review: approved
   open ───────────────────────────▶ in_progress ──────────▶ review ──────────▶ qa
    ▲                                     ▲                      │                │
    │                                     │ changes_requested    │                │ passed
    │                                     └──────────────────────┘                ▼
    │                                                                            done
    └──────────────────────── (rejected, from review) ──────────────────────▶ rejected
```

`update_response(section)` is the single mutation that advances a request:

| section written | verdict | request moves to |
|---|---|---|
| engineer | — | `review` |
| review | `approved` | `qa` |
| review | `changes_requested` | `in_progress` (back to engineer) |
| review | `rejected` | `rejected` |
| qa | `passed` | `done` |
| qa | `failed` | `in_progress` (back to engineer) |

Each request is a directory under the workspace root:
`<root>/<id>/request.yml` (the task) + `response.yml` (engineer/review/qa
sections as they're filled in). Messages are one file each under
`<root>/messages/<id>.yml`.

---

## The three binaries

### `forge-workspace` — CLI to drive the board by hand

Handy for seeding, inspection, and tests (no agents, no tokens). Root comes from
`--root DIR` or `$FORGE_WORKSPACE_DIR` (default `./.forge-workspace`).

```sh
forge-workspace create   --title T [--desc D] [--criteria C]...   # -> open
forge-workspace list     [--status open|in_progress|review|qa|done|rejected]
forge-workspace get      <id>
forge-workspace claim    <id> <agent>                             # open -> in_progress
forge-workspace engineer <id> [--files a,b] [--notes N]           # -> review
forge-workspace review   <id> --result approved|changes_requested|rejected
forge-workspace qa       <id> --result passed|failed
```

### `forge-workspace-mcp` — the workspace as MCP tools

A minimal stdio MCP server (hand-rolled newline-delimited JSON-RPC, no rmcp) that
exposes the board to agents. Root from `$FORGE_WORKSPACE_DIR`. Wire it into
Forge's `.mcp.json`:

```json
{ "mcpServers": { "forge-workspace": {
    "command": "forge-workspace-mcp",
    "env": { "FORGE_WORKSPACE_DIR": "/abs/path/.forge-workspace" } } } }
```

**Tools:** `create_request`, `claim_request`, `get_request`, `list_requests`,
`submit_engineer_work`, `submit_review`, `submit_qa`, `send_message`,
`get_inbox`.

### `forge-workspace-run` — the orchestrator (the "team brain")

A resident event loop: for every pending request it spawns a `forge` subprocess
as the right role (role SOP as prompt, workspace MCP connected), then re-reads
state and reacts.

```sh
forge-workspace-run --project DIR [--workspace DIR] [--forge PATH] \
    [--goal "<objective>"] [--plan-only] \
    [--concurrent N] [--max-attempts N] [--poll-secs N] \
    [--agent-timeout-secs N] [--alert-to INBOX] \
    [--daemon] [--dry-run] [--isolate-mcp]
```

| flag | default | meaning |
|---|---|---|
| `--project` | `.` | the codebase agents work in |
| `--workspace` | `<project>/.forge-workspace` | where the board lives |
| `--forge` | next to this binary | the `forge` binary to spawn |
| `--goal "…"` | — | a coordinator decomposes it into requests at startup (⑧) |
| `--plan-only` | off | run only the coordinator, print the board, exit |
| `--concurrent` | 1 | how many requests to work in parallel |
| `--max-attempts` | 4 | polls in one status before a request is parked as stuck |
| `--poll-secs` | 3 | how often to re-scan the board |
| `--agent-timeout-secs` | 300 | kill a subprocess that outlives this (hung-agent recovery, ⑨) |
| `--alert-to` | `human` | inbox that stuck alerts are pushed to (⑨) |
| `--daemon` | off | keep running and pick up new requests |
| `--dry-run` | off | schedule without spawning agents (logic tests, no tokens) |
| `--isolate-mcp` | off | run agents against an isolated base_path exposing ONLY the workspace MCP |

---

## Quick start (full pipeline from one goal)

```sh
# build everything
cargo build -p forge_workspace

# hand a goal to the team; the coordinator plans, the pipeline delivers
./target/debug/forge-workspace-run \
    --project /path/to/repo \
    --workspace /path/to/repo/.forge-workspace \
    --goal "Harden calc.py: reject non-numeric args with TypeError, raise \
            ValueError on divide-by-zero, add subtract(); include pytest tests." \
    --isolate-mcp --concurrent 1
```

The coordinator explores the repo and creates focused requests; the pipeline then
runs each `engineer → reviewer → qa → done` on its own. Watch it live in the web
board (below).

---

## The roles (SOPs)

Each role is a Markdown SOP in [`roles/`](roles/), injected as the agent's prompt
along with a live **topology** snapshot (the team roster + the current board) so
every agent knows who's upstream/downstream and what state everything is in.

- **[coordinator](roles/coordinator.md)** (Lead) — turns a goal into focused,
  testable requests. Plans and delegates only; never writes code.
- **[engineer](roles/engineer.md)** — claims open work, checks the inbox for
  rework tickets, implements against the acceptance criteria, submits.
- **[reviewer](roles/reviewer.md)** — reviews against criteria + quality +
  OWASP security + performance; verifies each finding against a concrete failure
  before reporting; on `changes_requested` messages the engineer what to fix.
- **[qa](roles/qa.md)** — verifies each acceptance criterion by actually running
  tests, then passes or fails.

---

## Design notes

**Why an outer layer.** Mirroring the pattern from `aiwatching/forge` (which
drives Claude Code), the orchestrator drives `forge -p` subprocesses. Keeping the
coordination logic *outside* the Forge crates means no merge pain with upstream.

**`--isolate-mcp`.** Spawned agents otherwise load every globally-configured MCP
server (github/gmail/slack/… via npx) — slow startup and flaky tool registration.
Isolate mode points `FORGE_CONFIG` at `<workspace>/.forge-home/`, which
**symlinks** the real provider credentials (never copies) but ships a
workspace-only `.mcp.json`. Result: faster, and the workspace tools register
reliably.

**Monitoring & recovery (⑨).** A subprocess that outlives `--agent-timeout-secs`
is killed so it can't wedge a concurrency slot forever; its request is retried
next poll. A request that stays stuck past `--max-attempts` is parked, and a
single ticket is pushed to the `--alert-to` inbox on the bus.

**Message bus is a pure blackboard.** `get_inbox` marks messages read (so an
agent polling `unread_only` sees each once); `list_messages` is a pure read for
dashboards.

---

## Web board

`forge_web` surfaces the team's board as a card in the Forge web UI (`forge
serve`): request counts per status, the full request list, and recent stuck /
rework alert tickets. Point it at a workspace with the card's inline path input,
or `$FORGE_WORKSPACE_DIR` / `PUT /api/workspace-dir`.

---

## Testing without burning tokens

- **Logic** — `--dry-run` exercises scheduling, concurrency, stuck detection, and
  termination with no agents; unit tests cover the state machine and bus.
- **Recovery** — point `--forge` at a script that hangs (`sleep 100`) with a small
  `--agent-timeout-secs` to see the kill/retry/alert path, no tokens.
- **Agent behaviour** — real `forge -p` runs (these cost tokens); watch them via
  the web board or by tailing the orchestrator log.

```sh
cargo test -p forge_workspace
```
