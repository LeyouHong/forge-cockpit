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
//! raw PTY output as binary frames.
//!
//! Authorization rides in the WebSocket *subprotocol*, not the query string.
//! The browser's WebSocket API cannot set an `Authorization` header, but it can
//! offer subprotocols — and unlike a URL, those never reach devtools' network
//! bar as a visible parameter, a proxy access log, or a copied-and-pasted link.
//! The client offers `["forge-terminal", "<token>"]`; the server checks the
//! token and selects `forge-terminal`, so the secret is never echoed back.

use std::io::{Read, Write};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;

use crate::AppState;
use forge_api::API;

/// The subprotocol the server selects. The token is offered alongside it and is
/// deliberately *not* the one selected — selecting it would echo the secret back
/// in the handshake response.
const TERM_PROTOCOL: &str = "forge-terminal";

#[derive(Deserialize)]
pub(crate) struct TermQuery {
    session: String,
}

/// Only sessions the orchestrator created are attachable — this route must
/// not become a generic "attach to any tmux on the box" endpoint.
fn valid_session(name: &str) -> bool {
    name.strip_prefix("forge-team-").is_some_and(|id| {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// True when the client's offered subprotocols include the session token.
fn offers_token(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|list| list.split(',').any(|p| crate::secret_eq(p.trim(), token)))
}

/// GET /ws/terminal?session=forge-team-<id> — upgrade to the bridge.
///
/// Authorized by the token offered as a subprotocol (see the module docs).
pub(crate) async fn terminal_ws<A: API>(
    State(state): State<AppState<A>>,
    headers: HeaderMap,
    Query(q): Query<TermQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !offers_token(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !valid_session(&q.session) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    ws.protocols([TERM_PROTOCOL])
        .on_upgrade(move |socket| bridge(socket, q.session))
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
    use pretty_assertions::assert_eq;

    use super::*;

    fn headers_offering(protocols: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, protocols.parse().unwrap());
        h
    }

    /// This socket hands out a shell. Only the exact token opens it.
    #[test]
    fn test_offers_token_accepts_only_the_real_token() {
        let tok = "5b1f0c9e-7a2d-4c3b-9f10-2b8e6d4a1c77";

        assert_eq!(offers_token(&headers_offering(&format!("forge-terminal, {tok}")), tok), true);
        assert_eq!(offers_token(&headers_offering(tok), tok), true);

        assert_eq!(offers_token(&headers_offering("forge-terminal"), tok), false);
        assert_eq!(offers_token(&headers_offering("forge-terminal, wrong"), tok), false);
        // A prefix of the token must not pass.
        assert_eq!(offers_token(&headers_offering(&tok[..8]), tok), false);
        // No header at all.
        assert_eq!(offers_token(&HeaderMap::new(), tok), false);
    }

    /// The token must never be echoed back in the handshake — the server picks
    /// `forge-terminal` out of the offered list, not the secret.
    #[tokio::test]
    async fn test_handshake_selects_forge_terminal_and_never_echoes_the_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // The upgrade contract we depend on, exercised against a live axum
        // server: offer [forge-terminal, <token>], expect 101 with
        // forge-terminal selected. If axum ever answered differently the
        // browser would close the socket and the terminal pane would go dark.
        async fn upgrade(ws: WebSocketUpgrade) -> Response {
            ws.protocols([TERM_PROTOCOL]).on_upgrade(|_| async {})
        }
        let app = axum::Router::new().route("/ws", axum::routing::get(upgrade));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let token = "5b1f0c9e-7a2d-4c3b-9f10-2b8e6d4a1c77";
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Protocol: {TERM_PROTOCOL}, {token}\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 1024];
        let n = sock.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);

        assert_eq!(resp.contains("101"), true, "handshake was refused:\n{resp}");
        assert_eq!(
            resp.to_lowercase().contains(&format!("sec-websocket-protocol: {TERM_PROTOCOL}")),
            true,
            "server did not select {TERM_PROTOCOL}:\n{resp}"
        );
        assert_eq!(resp.contains(token), false, "the token was echoed back in the handshake:\n{resp}");
    }

    #[test]
    fn test_valid_session_names() {
        assert_eq!(valid_session("forge-team-engineer"), true);
        assert_eq!(valid_session("forge-team-eng_2"), true);
        assert_eq!(valid_session("forge-team-"), false);
        assert_eq!(valid_session("main"), false);
        assert_eq!(valid_session("forge-team-a;rm"), false);
    }
}
