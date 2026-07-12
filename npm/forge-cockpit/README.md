# forge-cockpit

An AI coding agent with a **browser cockpit** — the same agent you drive from the
terminal, plus platform dashboards and one-click integrations
(GitHub · Jira · Sentry · Slack · Gmail · Google Calendar). **Bring your own model
API key.** A fork of [Forge](https://github.com/antinomyhq/forge), Apache-2.0.

<p align="center">
  <img src="https://raw.githubusercontent.com/LeyouHong/forge-cockpit/main/docs/img/cockpit.png" alt="forge-cockpit web UI" width="820">
</p>

## Install

```bash
npm install -g forge-cockpit
# or run without installing:
npx forge-cockpit --help
```

On install, the prebuilt binary for your platform (macOS arm64, Linux x64/arm64,
Windows x64) is downloaded from the matching GitHub Release — nothing is compiled
on your machine. (Install with scripts enabled, i.e. not `--ignore-scripts`.)

## Bring your own key

No hosted account is bundled or required. On first run, log in with **your own**
model provider key:

```bash
forge-cockpit provider login     # pick OpenAI / Anthropic / OpenRouter / …
forge-cockpit                    # interactive agent in your terminal
forge-cockpit serve              # open the browser cockpit
```

## The cockpit

`forge-cockpit serve` opens a local browser UI (bound to `127.0.0.1`, gated by a
per-run token):

- **💬 Chat** — the full agent: streaming responses, tool-call chips, resumable
  turns (refresh mid-run without losing progress).
- **📋 Dashboard** — read-only boards over your connected platforms.
- **🧩 Integrations** — one-click connect to MCP servers (read **and** write).

And a full orchestration stack:

- **🧬 Pipelines** — visual DAG workflows (agent + shell nodes); the chat agent
  can discover and run them itself (`pipeline_list` / `pipeline_run`).
- **🤝 Team** — a resident multi-agent team on an editable canvas
  (PM → Architect → Engineer → Reviewer → QA), with approval gates, per-role
  models, and a project code browser.
- **⏰ Schedules** — cron/interval/one-shot triggers → a pipeline or prompt →
  optional webhook / email delivery.
- **📊 Usage** — token & cost of your agents, priced at your provider's rate.
- **🧩 Crafts** — describe a mini-app; a builder agent writes a self-contained
  HTML page into your project and renders it in a tab.

<p align="center">
  <img src="https://raw.githubusercontent.com/LeyouHong/forge-cockpit/main/docs/img/dashboard.png" alt="Dashboard" width="820">
</p>
<p align="center">
  <img src="https://raw.githubusercontent.com/LeyouHong/forge-cockpit/main/docs/img/integrations.png" alt="Integrations" width="820">
</p>

## Docs & source

Full documentation, architecture, and build-from-source instructions:
**https://github.com/LeyouHong/forge-cockpit**

## Notes

- **Linux:** the binary dynamically links OpenSSL (`libssl`) for IMAP email
  support; present on virtually all distros (`libssl3` if missing).
- **Intel Mac (x64):** no prebuilt binary yet — build from source.
- License: Apache-2.0. A community fork; "Forge" and "forgecode" are trademarks
  of their respective owners, and this package is not affiliated with them.
