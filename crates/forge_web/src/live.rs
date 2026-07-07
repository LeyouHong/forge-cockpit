//! Resumable chat turns.
//!
//! A chat turn runs as a background task feeding an event log + broadcast
//! channel keyed by conversation id (the same pattern squad runs use). The
//! browser attaches to the stream via a separate SSE endpoint, so a page
//! refresh mid-turn re-attaches (replay + live) instead of losing the agent's
//! progress. Stopping is an explicit endpoint that aborts the producer task —
//! disconnecting a viewer no longer kills the turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as AxPath, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use forge_api::API;
use forge_domain::{
    ChatRequest, ChatResponse, Conversation, ConversationId, Event as DomainEvent, TokenCount,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::dto::ChatEventDto;
use crate::{AppError, AppState};

fn short_id() -> String {
    ConversationId::generate().into_string().chars().take(8).collect()
}

/// How long a finished turn stays attachable (covers "completed right as the
/// client was re-attaching" races); swept lazily.
const FINISHED_TTL: Duration = Duration::from_secs(60);

pub(crate) struct ActiveTurn {
    pub log: Mutex<Vec<String>>,
    pub tx: broadcast::Sender<String>,
    pub abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
    pub done: AtomicBool,
    pub finished_at: std::sync::Mutex<Option<Instant>>,
    pub seq: AtomicU64,
}

pub(crate) type TurnRegistry = Arc<Mutex<HashMap<String, Arc<ActiveTurn>>>>;

async fn emit(turn: &ActiveTurn, mut ev: Value) {
    let seq = turn.seq.fetch_add(1, Ordering::SeqCst);
    if let Value::Object(m) = &mut ev {
        m.insert("seq".into(), json!(seq));
    }
    let s = ev.to_string();
    turn.log.lock().await.push(s.clone());
    let _ = turn.tx.send(s);
}

/// Sentinel pushed on the broadcast when a turn ends so attached SSE streams
/// close (the sender lives in the registry, so `Closed` never fires on its own).
pub(crate) const DONE_SENTINEL: &str = "__done__";

fn finish(turn: &ActiveTurn) {
    turn.done.store(true, Ordering::SeqCst);
    *turn.finished_at.lock().unwrap() = Some(Instant::now());
    let _ = turn.tx.send(DONE_SENTINEL.to_string());
}

/// Drops finished turns past their TTL.
async fn sweep(turns: &TurnRegistry) {
    turns.lock().await.retain(|_, t| {
        match *t.finished_at.lock().unwrap() {
            Some(at) => at.elapsed() < FINISHED_TTL,
            None => true,
        }
    });
}

/// Sums the per-request usage entries of a conversation into one total.
pub(crate) async fn conversation_usage<A: API>(api: &A, id: &ConversationId) -> Option<Value> {
    let conversation = api.conversation(id).await.ok().flatten()?;
    let messages = &conversation.context.as_ref()?.messages;
    let n = |t: TokenCount| match t {
        TokenCount::Actual(v) | TokenCount::Approx(v) => v,
    };
    let (mut p, mut c, mut total, mut cost) = (0usize, 0usize, 0usize, 0f64);
    let mut has_cost = false;
    for entry in messages {
        if let Some(u) = &entry.usage {
            p += n(u.prompt_tokens);
            c += n(u.completion_tokens);
            total += n(u.total_tokens);
            if let Some(x) = u.cost {
                cost += x;
                has_cost = true;
            }
        }
    }
    Some(json!({
        "type": "usage",
        "prompt_tokens": p,
        "completion_tokens": c,
        "total_tokens": total,
        "cost": if has_cost { Some(cost) } else { None },
    }))
}

// ---------------------------------------------------------------------------
// POST /api/chat — start a turn (returns immediately; attach via /live)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ImageInput {
    base64: String,
    mime: String,
}

#[derive(Deserialize)]
pub(crate) struct ChatBody {
    conversation_id: String,
    message: String,
    #[serde(default)]
    images: Vec<ImageInput>,
}

pub(crate) async fn start_turn<A: API + 'static>(
    State(state): State<AppState<A>>,
    Json(body): Json<ChatBody>,
) -> Result<Json<Value>, AppError> {
    let conversation_id = ConversationId::parse(&body.conversation_id)?;
    let key = conversation_id.into_string();
    sweep(&state.turns).await;
    {
        let turns = state.turns.lock().await;
        if let Some(t) = turns.get(&key) {
            if !t.done.load(Ordering::SeqCst) {
                return Err(AppError::bad_request("a turn is already running for this conversation"));
            }
        }
    }

    // Ensure the conversation exists (mirrors the old chat handler).
    if state.api.conversation(&conversation_id).await?.is_none() {
        state
            .api
            .upsert_conversation(Conversation::new(conversation_id))
            .await?;
    }

    // Forge's pipeline picks up attachments by parsing `@[path]` tags in the
    // message text (Event.attachments is not consumed by the orchestrator), so
    // uploaded images are written to temp files and referenced by tag.
    let mut message = body.message.clone();
    if !body.images.is_empty() {
        use base64::Engine;
        let dir = std::env::temp_dir().join("forge-web-uploads");
        std::fs::create_dir_all(&dir).map_err(|e| AppError::bad_request(format!("temp dir: {e}")))?;
        for img in &body.images {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(img.base64.as_bytes())
                .map_err(|e| AppError::bad_request(format!("bad image data: {e}")))?;
            let ext = match img.mime.as_str() {
                "image/jpeg" | "image/jpg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => "png",
            };
            let path = dir.join(format!("{}.{ext}", short_id()));
            std::fs::write(&path, bytes).map_err(|e| AppError::bad_request(format!("save image: {e}")))?;
            message.push_str(&format!("\n@[{}]", path.display()));
        }
    }
    let event = DomainEvent::new(message);
    let request = ChatRequest::new(event, conversation_id);

    let (tx, _rx) = broadcast::channel::<String>(1024);
    let turn = Arc::new(ActiveTurn {
        log: Mutex::new(Vec::new()),
        tx,
        abort: std::sync::Mutex::new(None),
        done: AtomicBool::new(false),
        finished_at: std::sync::Mutex::new(None),
        seq: AtomicU64::new(0),
    });
    state.turns.lock().await.insert(key.clone(), turn.clone());

    let api = state.api.clone();
    let turn2 = turn.clone();
    let user_text = body.message;
    let handle = tokio::spawn(async move {
        // First event re-renders the user bubble on re-attach after a refresh.
        emit(&turn2, json!({ "type": "user", "text": user_text })).await;
        match api.chat(request).await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(resp) => {
                            // Signal the orchestrator so tool-using turns don't deadlock.
                            if let ChatResponse::ToolCallStart { notifier, .. } = &resp {
                                notifier.notify_one();
                            }
                            let v = serde_json::to_value(ChatEventDto::from(&resp))
                                .unwrap_or_else(|_| json!({ "type": "error", "message": "serialize failed" }));
                            emit(&turn2, v).await;
                        }
                        Err(e) => {
                            let v = serde_json::to_value(ChatEventDto::Error { message: format!("{e:?}") })
                                .unwrap_or_default();
                            emit(&turn2, v).await;
                        }
                    }
                }
                if let Some(usage) = conversation_usage(api.as_ref(), &conversation_id).await {
                    emit(&turn2, usage).await;
                }
            }
            Err(e) => {
                emit(&turn2, json!({ "type": "error", "message": format!("{e:?}") })).await;
            }
        }
        finish(&turn2);
    });
    *turn.abort.lock().unwrap() = Some(handle.abort_handle());

    Ok(Json(json!({ "ok": true, "conversation_id": key })))
}

// ---------------------------------------------------------------------------
// GET /api/chat/{conv}/live — attach (replay + live)
// ---------------------------------------------------------------------------

pub(crate) async fn turn_live<A: API + 'static>(
    State(state): State<AppState<A>>,
    AxPath(conv): AxPath<String>,
) -> Response {
    sweep(&state.turns).await;
    let turn = match state.turns.lock().await.get(&conv).cloned() {
        Some(t) => t,
        None => return AppError::not_found("no active turn").into_response(),
    };
    // Subscribe before snapshotting the log; the client dedups by seq.
    let mut rx = turn.tx.subscribe();
    let backlog = turn.log.lock().await.clone();
    let done = turn.done.load(Ordering::SeqCst);
    let stream = async_stream::stream! {
        for s in backlog {
            yield Ok::<_, std::convert::Infallible>(SseEvent::default().data(s));
        }
        if !done {
            loop {
                match rx.recv().await {
                    Ok(s) if s == DONE_SENTINEL => break,
                    Ok(s) => yield Ok(SseEvent::default().data(s)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    };
    Sse::new(stream).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/chat/{conv}/stop — abort the producer (stops the agent)
// ---------------------------------------------------------------------------

pub(crate) async fn turn_stop<A: API>(
    State(state): State<AppState<A>>,
    AxPath(conv): AxPath<String>,
) -> Result<Json<Value>, AppError> {
    let turn = state
        .turns
        .lock()
        .await
        .get(&conv)
        .cloned()
        .ok_or_else(|| AppError::not_found("no active turn"))?;
    if let Some(h) = turn.abort.lock().unwrap().take() {
        h.abort();
    }
    emit(&turn, json!({ "type": "interrupt", "reason": "stopped by user" })).await;
    finish(&turn);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// GET /api/conversations/{id}/usage — aggregate usage for the topbar
// ---------------------------------------------------------------------------

pub(crate) async fn get_usage<A: API>(
    State(state): State<AppState<A>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Value>, AppError> {
    let id = ConversationId::parse(&id)?;
    Ok(Json(
        conversation_usage(state.api.as_ref(), &id)
            .await
            .unwrap_or_else(|| json!({ "type": "usage", "total_tokens": 0 })),
    ))
}
