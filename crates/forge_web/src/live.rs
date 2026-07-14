//! Resumable chat turns.
//!
//! A chat turn runs as a background task feeding an event log + broadcast
//! channel keyed by conversation id (the same pattern squad runs use). The
//! browser attaches to the stream via a separate SSE endpoint, so a page
//! refresh mid-turn re-attaches (replay + live) instead of losing the agent's
//! progress. Stopping is an explicit endpoint that aborts the producer task —
//! disconnecting a viewer no longer kills the turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as AxPath, State};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use forge_api::API;
use forge_domain::{
    ChatRequest, ChatResponse, ContextMessage, Conversation, ConversationId, Event as DomainEvent,
    EventValue, Role, TokenCount,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
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
    /// A short label for the tasks panel (the user's prompt, collapsed).
    pub prompt: String,
    pub started: Instant,
    pub started_at_ms: u64,
    pub errored: AtomicBool,
    pub stopped: AtomicBool,
}

pub(crate) type TurnRegistry = Arc<Mutex<HashMap<String, Arc<ActiveTurn>>>>;

/// A finished task, kept for the tasks panel. The registry only holds turns
/// briefly after completion (FINISHED_TTL), so history is recorded separately;
/// it is in-memory and scoped to this `forge serve` run.
#[derive(Clone, Serialize)]
pub(crate) struct TaskRecord {
    pub conversation_id: String,
    pub prompt: String,
    pub started_at_ms: u64,
    pub duration_secs: u64,
    /// `completed`, `stopped`, or `error`.
    pub status: String,
}

/// Most-recent-first, capped at [`HISTORY_CAP`].
pub(crate) type TaskHistory = Arc<std::sync::Mutex<VecDeque<TaskRecord>>>;

const HISTORY_CAP: usize = 50;

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

fn finish(turn: &ActiveTurn, key: &str, history: &TaskHistory) {
    // Both the producer task and the stop endpoint call this; only the first
    // caller records the task (`done` doubles as the once-guard).
    if turn.done.swap(true, Ordering::SeqCst) {
        return;
    }
    *crate::lock(&turn.finished_at) = Some(Instant::now());
    let _ = turn.tx.send(DONE_SENTINEL.to_string());

    let status = if turn.stopped.load(Ordering::SeqCst) {
        "stopped"
    } else if turn.errored.load(Ordering::SeqCst) {
        "error"
    } else {
        "completed"
    };
    let mut h = crate::lock(&history);
    h.push_front(TaskRecord {
        conversation_id: key.to_string(),
        prompt: turn.prompt.clone(),
        started_at_ms: turn.started_at_ms,
        duration_secs: turn.started.elapsed().as_secs(),
        status: status.to_string(),
    });
    h.truncate(HISTORY_CAP);
}

/// Drops finished turns past their TTL.
async fn sweep(turns: &TurnRegistry) {
    turns.lock().await.retain(|_, t| {
        match *crate::lock(&t.finished_at) {
            Some(at) => at.elapsed() < FINISHED_TTL,
            None => true,
        }
    });
}

/// Removes the injected TODO-context block (see [`crate::board::todos_context`])
/// from the persisted conversation once a turn is done. The model needed it
/// in-request, but it shouldn't linger in the stored history — otherwise it
/// resurfaces every time the user reopens or exports the conversation.
///
/// A user message carries the block in two places that must both be
/// stripped: `content` (the rendered/template-wrapped text) and
/// `raw_content` (the pre-template `EventValue` `forge_web::dto::MessageDto`
/// prefers when displaying user turns). Truncating only `content` is a no-op
/// as far as the UI/export is concerned, since it reads `raw_content` first.
async fn redact_todo_context<A: API>(api: &A, id: &ConversationId) {
    let Ok(Some(mut conversation)) = api.conversation(id).await else { return };
    let Some(ctx) = conversation.context.as_mut() else { return };
    let mut changed = false;
    for entry in ctx.messages.iter_mut() {
        let ContextMessage::Text(t) = &mut entry.message else { continue };
        if t.role != Role::User {
            continue;
        }
        if let Some(idx) = t.content.find(crate::board::TODOS_CONTEXT_MARKER) {
            t.content.truncate(idx);
            changed = true;
        }
        if let Some(raw) = t.raw_content.as_ref().and_then(|v| v.as_user_prompt())
            && let Some(idx) = raw.find(crate::board::TODOS_CONTEXT_MARKER)
        {
            t.raw_content = Some(EventValue::from(&raw[..idx]));
            changed = true;
        }
    }
    if changed {
        let _ = api.upsert_conversation(conversation).await;
    }
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
    // When the user references their TODO list, attach the web panel's list:
    // the agent's own todo tools are session-scoped and can't see it. Kept out
    // of the echoed user bubble (which renders body.message).
    if body.message.to_lowercase().contains("todo") || body.message.contains("待办") {
        message.push_str(&crate::board::todos_context());
    }
    let event = DomainEvent::new(message);
    let request = ChatRequest::new(event, conversation_id);

    // Collapse the prompt to one trimmed line for the tasks panel.
    let prompt = {
        let line = body.message.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            "(image)".to_string()
        } else {
            crate::truncate_chars(&line, 120)
        }
    };
    let (tx, _rx) = broadcast::channel::<String>(1024);
    let turn = Arc::new(ActiveTurn {
        log: Mutex::new(Vec::new()),
        tx,
        abort: std::sync::Mutex::new(None),
        done: AtomicBool::new(false),
        finished_at: std::sync::Mutex::new(None),
        seq: AtomicU64::new(0),
        prompt,
        started: Instant::now(),
        started_at_ms: unix_ms(),
        errored: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
    });
    state.turns.lock().await.insert(key.clone(), turn.clone());

    let api = state.api.clone();
    let turn2 = turn.clone();
    let history = state.history.clone();
    let key2 = key.clone();
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
                            turn2.errored.store(true, Ordering::SeqCst);
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
                turn2.errored.store(true, Ordering::SeqCst);
                emit(&turn2, json!({ "type": "error", "message": format!("{e:?}") })).await;
            }
        }
        // The user message was persisted with the injected TODO block already
        // (user_prompt.rs renders it into the context before the model call),
        // so strip it now regardless of how the turn ended.
        redact_todo_context(api.as_ref(), &conversation_id).await;
        finish(&turn2, &key2, &history);
    });
    *crate::lock(&turn.abort) = Some(handle.abort_handle());

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
    if let Some(h) = crate::lock(&turn.abort).take() {
        h.abort();
    }
    turn.stopped.store(true, Ordering::SeqCst);
    emit(&turn, json!({ "type": "interrupt", "reason": "stopped by user" })).await;
    finish(&turn, &conv, &state.history);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// GET /api/tasks — running turns + recently finished ones, for the tasks panel
// ---------------------------------------------------------------------------

pub(crate) async fn list_tasks<A: API>(State(state): State<AppState<A>>) -> Json<Value> {
    sweep(&state.turns).await;
    let mut running = Vec::new();
    for (key, t) in state.turns.lock().await.iter() {
        if !t.done.load(Ordering::SeqCst) {
            running.push(json!({
                "conversation_id": key,
                "prompt": t.prompt,
                "started_at_ms": t.started_at_ms,
                "elapsed_secs": t.started.elapsed().as_secs(),
            }));
        }
    }
    // Newest first, matching the recent list.
    running.sort_by_key(|v| std::cmp::Reverse(v["started_at_ms"].as_u64().unwrap_or(0)));
    let recent: Vec<TaskRecord> = crate::lock(&state.history).iter().cloned().collect();
    Json(json!({ "running": running, "recent": recent }))
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
