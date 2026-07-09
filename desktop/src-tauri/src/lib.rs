//! Forge desktop shell (Tauri 2).
//!
//! A thin native window around the existing `forge serve` web app: on startup it
//! spawns `forge serve` (loopback + per-run token), reads the URL it prints, and
//! points a native webview at it. Everything else — the UI, the API, connectors,
//! OAuth (the loopback callback still works because the local server is running)
//! — is reused unchanged. The subprocess is killed when the window closes.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// Holds the spawned `forge serve` process so it can be killed on exit.
struct ForgeServer(Mutex<Option<Child>>);

/// Starts `forge serve` and returns the child plus the local URL (with token) it
/// printed on stdout. The binary is `forge` on PATH, overridable via `FORGE_BIN`.
fn start_forge() -> Result<(Child, String), String> {
    let bin = std::env::var("FORGE_BIN").unwrap_or_else(|_| "forge".to_string());
    let mut child = Command::new(&bin)
        .args(["serve", "--port", "0", "--no-open"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start `{bin} serve`: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout from forge serve")?;
    let mut url = String::new();
    // The ready line is the one carrying the token (an earlier line echoes the
    // requested ":0" address, which must not be picked).
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if line.contains("?token=") {
            if let Some(i) = line.find("http://127.0.0.1") {
                url = line[i..].trim().to_string();
                break;
            }
        }
    }
    if url.is_empty() {
        let _ = child.kill();
        return Err("forge serve did not print a local URL".to_string());
    }
    Ok((child, url))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let (child, url) =
                start_forge().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            app.manage(ForgeServer(Mutex::new(Some(child))));
            let parsed: tauri::Url = url.parse()?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
                .title("Forge")
                .inner_size(1200.0, 820.0)
                .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::Destroyed) {
                if let Some(state) = window.app_handle().try_state::<ForgeServer>() {
                    if let Some(mut child) = state.0.lock().unwrap().take() {
                        let _ = child.kill();
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running the Forge desktop shell");
}
