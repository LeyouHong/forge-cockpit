# Forge desktop shell (Tauri 2)

A thin native window around the existing `forge serve` web app. On startup it
spawns `forge serve` (loopback + a per-run token), reads the URL it prints, and
points a native webview at it. The UI, API, connectors, and OAuth are all reused
unchanged — OAuth's loopback `/oauth/callback` still works because the local
server is running behind the window.

## Prerequisites

- The `forge` binary on your `PATH` (or set `FORGE_BIN=/path/to/forge`).
- Node + npm (only for the Tauri CLI, used to package a `.app`).
- macOS/Windows/Linux system webview (WebKit / WebView2 / webkit2gtk).

## Run (dev)

The simplest way — no Tauri CLI needed at runtime, since the binary embeds
everything:

```sh
cd desktop/src-tauri
FORGE_BIN=../../target/debug/forge cargo run
```

(or just `cargo run` if `forge` is on your `PATH`.)

## Package a distributable app

```sh
cd desktop
npm install            # installs @tauri-apps/cli
FORGE_BIN=forge npx tauri build   # produces a .app / .dmg (macOS), etc.
```

## Notes

- The window is created at runtime pointing at `http://127.0.0.1:<port>/?token=…`
  from the spawned server; `frontend/index.html` is only a build-time placeholder.
- The spawned `forge serve` is killed when the window closes.
- **Custom-scheme OAuth** (`forge://oauth/callback`) is a future enhancement for a
  fully native flow; it isn't required here because the loopback server handles
  the redirect while the app is running.
