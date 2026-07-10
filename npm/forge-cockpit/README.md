# forge-cockpit

An AI coding agent with a **browser cockpit** — the same agent you drive from the
terminal, plus read-only platform dashboards and one-click MCP integrations
(GitHub · Jira · Sentry · Slack · Gmail · Google Calendar). A fork of
[Forge](https://forgecode.dev), Apache-2.0.

## Install

```bash
npm install -g forge-cockpit
# or run without installing:
npx forge-cockpit --help
```

The right prebuilt binary for your platform (macOS/Linux/Windows, x64/arm64) is
pulled in automatically via an optional dependency — nothing is compiled on your
machine.

## Bring your own key

forge-cockpit does **not** ship or default to any hosted account. On first run,
log in with **your own** model provider key:

```bash
forge-cockpit provider login     # pick OpenAI / Anthropic / OpenRouter / …
forge-cockpit                    # start the interactive agent
forge-cockpit serve              # start the browser cockpit
```

## Notes

- **Linux:** the binary dynamically links OpenSSL (`libssl`) for IMAP email
  support. It's present on virtually all modern distros; install `libssl3` if
  missing.
- License: Apache-2.0. This is a community fork; "Forge" and "forgecode" are
  trademarks of their respective owners and are not affiliated with this package.
