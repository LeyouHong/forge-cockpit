//! Browser terminal for resident team members — an xterm.js pane in the
//! cockpit bridged over WebSocket to `tmux attach` running in a local PTY.
//!
//! This is what makes "a human can take over any member" true beyond the
//! machine's own keyboard: the same tmux session the orchestrator drives is
//! attached here, keystrokes and all. Closing the socket detaches the tmux
//! *client*; the member's session (and the agent in it) keeps running.
//!
//! Wire protocol: client → server is JSON text frames — `{"t":"i","d":"…"}`
//! for input, `{"t":"r","c":cols,"r":rows}` for resize; server → client is
//! raw PTY output as binary frames. The browser's WebSocket API can't set an
//! Authorization header, so this route authorizes via a `token` query param
//! checked against the same per-run bearer token as `/api/*`.

use std::io::{Read, Write};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;

use crate::AppState;
use forge_api::API;

#[derive(Deserialize)]
pub(crate) struct TermQuery {
    token: String,
    session: String,
}

/// Only sessions the orchestrator created are attachable — this route must
/// not become a generic "attach to any tmux on the box" endpoint.
fn valid_session(name: &str) -> bool {
    name.strip_prefix("forge-team-")
        .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// GET /ws/terminal?token=…&session=forge-team-<id> — upgrade to the bridge.
pub(crate) async fn terminal_ws<A: API>(
    State(state): State<AppState<A>>,
    Query(q): Query<TermQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if q.token != *state.token {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !valid_session(&q.session) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    ws.on_upgrade(move |socket| bridge(socket, q.session))
}

/// Close reasons the client shows verbatim — keep them human.
async fn fail(mut socket: WebSocket, msg: String) {
    let _ = socket.send(Message::Text(format!("\u{1}{msg}").into())).await;
    let _ = socket.close().await;
}

async fn bridge(socket: WebSocket, session: String) {
    // A dead pane must read as "member not resident", not a blank screen.
    let have = std::process::Command::new("tmux")
        .args(["has-session", "-t", &session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !have {
        return fail(
            socket,
            format!("no live terminal: tmux session `{session}` is not running (start the team first)"),
        )
        .await;
    }

    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(e) => return fail(socket, format!("openpty failed: {e}")).await,
    };
    let mut cmd = CommandBuilder::new("tmux");
    cmd.args(["attach", "-t", &session]);
    cmd.env("TERM", "xterm-256color");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => return fail(socket, format!("tmux attach failed: {e}")).await,
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return fail(socket, format!("pty reader: {e}")).await,
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => return fail(socket, format!("pty writer: {e}")).await,
    };

    // PTY output → channel (the PTY reader is blocking; it gets its own thread
    // and the channel end signals EOF when the tmux client exits/detaches).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Keystrokes → channel → PTY (writes are blocking too — tiny, but a full
    // PTY buffer must stall this thread, never the async runtime).
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    let (mut ws_tx, mut ws_rx) = socket.split();

    loop {
        tokio::select! {
            chunk = out_rx.recv() => match chunk {
                Some(bytes) => {
                    if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // tmux client exited (detach / session killed)
            },
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(txt))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
                    match v["t"].as_str() {
                        Some("i") => {
                            if let Some(d) = v["d"].as_str() {
                                let _ = in_tx.send(d.as_bytes().to_vec());
                            }
                        }
                        Some("r") => {
                            let (c, r) = (v["c"].as_u64().unwrap_or(80), v["r"].as_u64().unwrap_or(24));
                            let _ = pair.master.resize(PtySize {
                                rows: r.clamp(2, 500) as u16,
                                cols: c.clamp(2, 1000) as u16,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }

    // Detach: kill the tmux *client* we spawned. The session lives on.
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_valid_session_names() {
        assert_eq!(valid_session("forge-team-engineer"), true);
        assert_eq!(valid_session("forge-team-eng_2"), true);
        assert_eq!(valid_session("forge-team-"), false);
        assert_eq!(valid_session("main"), false);
        assert_eq!(valid_session("forge-team-a;rm"), false);
    }
}
