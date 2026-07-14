//! The message bus — reliable one-to-one delivery between agents.
//!
//! Semantics mirror the reference implementation's in-process AgentBus:
//! one-to-one (never broadcast), ACK confirmation from the receiver, retry on
//! no-ACK (30s, 3 attempts), dedup by message id, an **outbox** holding
//! messages for agents that are down (flushed when they come back), `caused_by`
//! causal tracing, and request/response so an agent can ask a peer a question
//! and wait for the answer.
//!
//! The transport is the difference. Our agents are separate processes (a tmux
//! pane each) and the orchestrator is a third — an in-memory EventEmitter
//! cannot span them, so every bus fact is a file under `<root>/messages/`
//! (one YAML per message). That also means the bus survives what the
//! reference's cannot: kill the orchestrator mid-flight and the log, the
//! outbox, and the pending ACKs are all still there afterwards.
//!
//! Delivery states: `pending` (sent) → `delivered` (in the recipient's inbox
//! read) → `acked` (recipient confirmed) — or `outboxed` (recipient down, held)
//! and `failed` (retries exhausted).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// No-ACK retry window, matching the reference bus.
pub const ACK_TIMEOUT_SECS: i64 = 30;
/// Delivery attempts before a message is marked `failed`.
pub const MAX_RETRIES: u32 = 3;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_id() -> String {
    let raw = forge_domain::ConversationId::generate().into_string();
    let short: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("msg-{short}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Fire-and-forget FYI. Delivery is not tracked.
    Notification,
    /// Expected to be acted on by the recipient; tracked to ACK.
    Ticket,
    /// A question awaiting a `Response` (see [`request`]).
    Request,
    /// The answer to a `Request` — carries `reply_to`.
    Response,
}

/// Where a message is in its delivery lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Sent; the recipient is up but hasn't read it yet.
    Pending,
    /// Held because the recipient is down; released by [`flush_outbox`].
    Outboxed,
    /// The recipient has read it (inbox poll).
    Delivered,
    /// The recipient confirmed it (ACK, or an answer to a request).
    Acked,
    /// Retries exhausted without an ACK.
    Failed,
}

/// Is an agent up? The orchestrator sets this from live tmux panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Alive,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub category: Category,
    pub body: String,
    pub created_at: String,
    /// Kept for the UI's unread badges; `true` once the recipient has read it.
    #[serde(default)]
    pub read: bool,
    #[serde(default = "default_status")]
    pub status: Status,
    /// Delivery attempts so far (retries after an ACK timeout).
    #[serde(default)]
    pub retries: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// When the current attempt was delivered — the ACK clock starts here.
    #[serde(default)]
    pub delivered_at: Option<String>,
    /// The message that caused this one — causal tracing across the bus.
    #[serde(default)]
    pub caused_by: Option<String>,
    /// For a `Response`: the `Request` it answers.
    #[serde(default)]
    pub reply_to: Option<String>,
}

fn default_status() -> Status {
    Status::Pending
}
fn default_max_retries() -> u32 {
    MAX_RETRIES
}

/// Options for [`send`] — all optional, mirroring the reference bus's
/// `send(from, to, type, payload, options)`.
#[derive(Debug, Clone, Default)]
pub struct SendOpts {
    pub caused_by: Option<String>,
    pub reply_to: Option<String>,
    pub max_retries: Option<u32>,
}

fn msg_dir(root: &Path) -> PathBuf {
    root.join("messages")
}
fn msg_path(root: &Path, id: &str) -> PathBuf {
    msg_dir(root).join(format!("{id}.yml"))
}
fn liveness_path(root: &Path) -> PathBuf {
    root.join(".bus-liveness.json")
}

fn write_message(root: &Path, m: &Message) -> Result<()> {
    std::fs::create_dir_all(msg_dir(root))?;
    let yaml = serde_yml::to_string(m).context("serialize message")?;
    std::fs::write(msg_path(root, &m.id), yaml).context("write message")?;
    Ok(())
}

fn read_message(root: &Path, id: &str) -> Option<Message> {
    std::fs::read_to_string(msg_path(root, id))
        .ok()
        .and_then(|t| serde_yml::from_str(&t).ok())
}

fn all_messages(root: &Path) -> Vec<Message> {
    let dir = msg_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<Message> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yml"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_yml::from_str::<Message>(&t).ok())
        .collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

// ─── Liveness (drives the outbox) ───────────────────────────────────────────

/// Record who is up. Coming back from `Down` flushes that agent's outbox — the
/// reference bus does exactly this on `setAgentStatus(alive)`.
pub fn set_liveness(root: &Path, agent: &str, status: Liveness) -> Result<()> {
    let path = liveness_path(root);
    let mut map: BTreeMap<String, Liveness> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let was = map.insert(agent.to_string(), status);
    std::fs::create_dir_all(root)?;
    std::fs::write(&path, serde_json::to_string_pretty(&map)?)?;
    // Any transition INTO alive flushes — including from unknown, which is the
    // first-boot case (mail sent to an agent that had never come up yet).
    if status == Liveness::Alive && was != Some(Liveness::Alive) {
        flush_outbox(root, agent)?;
    }
    Ok(())
}

/// Unknown agents are treated as `Down` — a message to an agent that has never
/// come up is held, not lost. (`human` is always reachable: a person reads the
/// UI, not a tmux pane.)
pub fn get_liveness(root: &Path, agent: &str) -> Liveness {
    if agent == "human" || agent == "orchestrator" {
        return Liveness::Alive;
    }
    std::fs::read_to_string(liveness_path(root))
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, Liveness>>(&s).ok())
        .and_then(|m| m.get(agent).copied())
        .unwrap_or(Liveness::Down)
}

/// Release everything held for `agent` (Outboxed → Pending).
pub fn flush_outbox(root: &Path, agent: &str) -> Result<usize> {
    let mut n = 0;
    for mut m in all_messages(root) {
        if m.to == agent && m.status == Status::Outboxed {
            m.status = Status::Pending;
            write_message(root, &m)?;
            n += 1;
        }
    }
    Ok(n)
}

// ─── Send / ACK ─────────────────────────────────────────────────────────────

/// Send a message. If the recipient is down it is held in the outbox instead of
/// being dropped.
pub fn send_with(
    root: &Path,
    from: &str,
    to: &str,
    body: &str,
    category: Category,
    opts: SendOpts,
) -> Result<Message> {
    let down = get_liveness(root, to) == Liveness::Down;
    let m = Message {
        id: new_id(),
        from: from.to_string(),
        to: to.to_string(),
        category,
        body: body.to_string(),
        created_at: now(),
        read: false,
        status: if down { Status::Outboxed } else { Status::Pending },
        retries: 0,
        max_retries: opts.max_retries.unwrap_or(MAX_RETRIES),
        delivered_at: None,
        caused_by: opts.caused_by,
        reply_to: opts.reply_to,
    };
    write_message(root, &m)?;
    Ok(m)
}

/// Send with default options (the long-standing signature).
pub fn send_message(root: &Path, from: &str, to: &str, body: &str, category: Category) -> Result<Message> {
    send_with(root, from, to, body, category, SendOpts::default())
}

/// The recipient confirms it has handled a message. Notifications don't need
/// this (they are fire-and-forget); tickets and requests do, and un-acked ones
/// are retried.
pub fn ack(root: &Path, agent: &str, message_id: &str) -> Result<bool> {
    let Some(mut m) = read_message(root, message_id) else {
        return Ok(false);
    };
    if m.to != agent {
        anyhow::bail!("message `{message_id}` is not addressed to `{agent}`");
    }
    m.status = Status::Acked;
    m.read = true;
    write_message(root, &m)?;
    Ok(true)
}

/// Retry every ticket/request whose ACK window has lapsed; mark the ones that
/// have burned their attempts as `failed`. Returns (retried, failed) ids.
///
/// "Retry" on a file bus means: put it back in front of the recipient — the
/// message returns to `pending` and unread, so the next inbox poll surfaces it
/// again. Called by the orchestrator on its poll loop.
pub fn retry_stale(root: &Path) -> (Vec<String>, Vec<String>) {
    let (mut retried, mut failed) = (Vec::new(), Vec::new());
    let now_ts = chrono::Utc::now();
    for mut m in all_messages(root) {
        if m.status != Status::Delivered || !matches!(m.category, Category::Ticket | Category::Request) {
            continue;
        }
        let Some(at) = m
            .delivered_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        if (now_ts - at.with_timezone(&chrono::Utc)).num_seconds() < ACK_TIMEOUT_SECS {
            continue;
        }
        if m.retries + 1 >= m.max_retries {
            m.status = Status::Failed;
            failed.push(m.id.clone());
        } else {
            m.retries += 1;
            m.status = Status::Pending;
            m.read = false; // resurface on the next inbox poll
            m.delivered_at = None;
            retried.push(m.id.clone());
        }
        let _ = write_message(root, &m);
    }
    (retried, failed)
}

// ─── Inbox ──────────────────────────────────────────────────────────────────

/// Read `agent`'s inbox (optionally only unread), oldest first.
///
/// Reading is delivery: returned messages move to `delivered` and start the ACK
/// clock. Notifications need no ACK, so they go straight to `acked`. Outboxed
/// messages are NOT returned — the agent is considered down until the
/// orchestrator says otherwise.
pub fn get_inbox(root: &Path, agent: &str, unread_only: bool) -> Result<Vec<Message>> {
    let mut out: Vec<Message> = all_messages(root)
        .into_iter()
        .filter(|m| m.to == agent && m.status != Status::Outboxed)
        .filter(|m| !unread_only || !m.read)
        .collect();
    for m in out.iter_mut() {
        let changed = !m.read || m.status == Status::Pending;
        if !changed {
            continue;
        }
        m.read = true;
        if m.status == Status::Pending {
            m.status = match m.category {
                // Fire-and-forget: reading IS the whole contract.
                Category::Notification | Category::Response => Status::Acked,
                Category::Ticket | Category::Request => {
                    m.delivered_at = Some(now());
                    Status::Delivered
                }
            };
        }
        let _ = write_message(root, m);
    }
    Ok(out)
}

/// List every message on the bus, newest first, WITHOUT changing anything.
/// For dashboards / monitoring — unlike [`get_inbox`] this is a pure read.
pub fn list_messages(root: &Path) -> Result<Vec<Message>> {
    let mut out = all_messages(root);
    out.reverse(); // newest first
    Ok(out)
}

// ─── Request / response ─────────────────────────────────────────────────────

/// Ask `to` a question and wait for its answer (the reference bus's
/// `request()`, which returns a Promise). Polls for a `Response` whose
/// `reply_to` is this request; `Ok(None)` on timeout.
///
/// The wait is a poll, not a blocked thread on an event — the answer will be
/// written by a different process.
pub fn request(
    root: &Path,
    from: &str,
    to: &str,
    body: &str,
    timeout: std::time::Duration,
) -> Result<Option<Message>> {
    let req = send_with(root, from, to, body, Category::Request, SendOpts::default())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(answer) = all_messages(root)
            .into_iter()
            .find(|m| m.category == Category::Response && m.reply_to.as_deref() == Some(req.id.as_str()))
        {
            // The answer IS the acknowledgement of the question.
            let _ = ack(root, to, &req.id);
            return Ok(Some(answer));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Answer a `Request`. Links the response to the question (`reply_to`) and
/// records the causal edge, so a trace can be reconstructed from the log.
pub fn respond(root: &Path, from: &str, request_id: &str, body: &str) -> Result<Message> {
    let Some(req) = read_message(root, request_id) else {
        anyhow::bail!("no such request `{request_id}`");
    };
    if req.category != Category::Request {
        anyhow::bail!("message `{request_id}` is not a request");
    }
    send_with(
        root,
        from,
        &req.from,
        body,
        Category::Response,
        SendOpts {
            reply_to: Some(request_id.to_string()),
            caused_by: Some(request_id.to_string()),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// An agent that has never been seen is down — its mail is held, not lost,
    /// and lands the moment it comes up.
    #[test]
    fn test_outbox_holds_for_down_agent_and_flushes_on_alive() {
        let d = tmp();
        let m = send_message(d.path(), "lead", "engineer-1", "start", Category::Ticket).unwrap();
        assert_eq!(m.status, Status::Outboxed);
        assert_eq!(get_inbox(d.path(), "engineer-1", false).unwrap().len(), 0);

        set_liveness(d.path(), "engineer-1", Liveness::Alive).unwrap();
        let inbox = get_inbox(d.path(), "engineer-1", true).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body, "start");
    }

    /// Reading delivers; only an explicit ACK closes a ticket.
    #[test]
    fn test_ticket_delivered_then_acked() {
        let d = tmp();
        set_liveness(d.path(), "eng-1", Liveness::Alive).unwrap();
        let m = send_message(d.path(), "rev-1", "eng-1", "rework the lockout", Category::Ticket).unwrap();
        assert_eq!(m.status, Status::Pending);

        let inbox = get_inbox(d.path(), "eng-1", true).unwrap();
        assert_eq!(inbox[0].status, Status::Delivered);
        assert!(inbox[0].delivered_at.is_some());

        assert!(ack(d.path(), "eng-1", &m.id).unwrap());
        assert_eq!(read_message(d.path(), &m.id).unwrap().status, Status::Acked);
    }

    /// A notification needs no ACK — reading is the whole contract.
    #[test]
    fn test_notification_acked_on_read() {
        let d = tmp();
        set_liveness(d.path(), "eng-1", Liveness::Alive).unwrap();
        send_message(d.path(), "board", "eng-1", "fyi", Category::Notification).unwrap();
        let inbox = get_inbox(d.path(), "eng-1", true).unwrap();
        assert_eq!(inbox[0].status, Status::Acked);
    }

    /// An un-acked ticket resurfaces after the ACK window, and fails once its
    /// attempts are spent.
    #[test]
    fn test_retry_resurfaces_then_fails() {
        let d = tmp();
        set_liveness(d.path(), "eng-1", Liveness::Alive).unwrap();
        let m = send_with(
            d.path(),
            "rev-1",
            "eng-1",
            "fix it",
            Category::Ticket,
            SendOpts { max_retries: Some(2), ..Default::default() },
        )
        .unwrap();
        get_inbox(d.path(), "eng-1", true).unwrap(); // delivered, not acked

        // Backdate delivery past the ACK window.
        let backdate = |id: &str| {
            let mut x = read_message(d.path(), id).unwrap();
            x.delivered_at =
                Some((chrono::Utc::now() - chrono::Duration::seconds(ACK_TIMEOUT_SECS + 5)).to_rfc3339());
            write_message(d.path(), &x).unwrap();
        };
        backdate(&m.id);

        let (retried, failed) = retry_stale(d.path());
        assert_eq!(retried, vec![m.id.clone()]);
        assert!(failed.is_empty());
        // Resurfaced: an unread poll sees it again.
        assert_eq!(get_inbox(d.path(), "eng-1", true).unwrap().len(), 1);

        backdate(&m.id);
        let (retried, failed) = retry_stale(d.path());
        assert!(retried.is_empty());
        assert_eq!(failed, vec![m.id.clone()], "second miss exhausts max_retries=2");
        assert_eq!(read_message(d.path(), &m.id).unwrap().status, Status::Failed);
    }

    /// Ask-and-wait: the answer carries reply_to/caused_by and closes the
    /// question.
    #[test]
    fn test_request_response_round_trip() {
        let d = tmp();
        set_liveness(d.path(), "arch-1", Liveness::Alive).unwrap();
        set_liveness(d.path(), "eng-1", Liveness::Alive).unwrap();
        let root = d.path().to_path_buf();

        // The peer answers asynchronously (a different process, in reality).
        let peer = std::thread::spawn(move || {
            for _ in 0..40 {
                if let Some(q) = get_inbox(&root, "arch-1", true)
                    .unwrap()
                    .into_iter()
                    .find(|m| m.category == Category::Request)
                {
                    respond(&root, "arch-1", &q.id, "use a bounded queue").unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        let answer = request(
            d.path(),
            "eng-1",
            "arch-1",
            "bounded or unbounded queue?",
            std::time::Duration::from_secs(10),
        )
        .unwrap()
        .expect("peer should answer");
        peer.join().unwrap();

        assert_eq!(answer.body, "use a bounded queue");
        assert_eq!(answer.to, "eng-1");
        assert!(answer.caused_by.is_some(), "causal edge back to the question");
        // The question is closed by its answer.
        let q = list_messages(d.path())
            .unwrap()
            .into_iter()
            .find(|m| m.category == Category::Request)
            .unwrap();
        assert_eq!(q.status, Status::Acked);
    }

    /// Dedup + one-shot reads still hold (the original contract).
    #[test]
    fn test_inbox_reads_once_and_is_per_recipient() {
        let d = tmp();
        set_liveness(d.path(), "engineer-1", Liveness::Alive).unwrap();
        set_liveness(d.path(), "qa-1", Liveness::Alive).unwrap();
        send_message(d.path(), "lead", "engineer-1", "a", Category::Notification).unwrap();
        send_message(d.path(), "lead", "qa-1", "b", Category::Notification).unwrap();
        assert_eq!(get_inbox(d.path(), "engineer-1", true).unwrap().len(), 1);
        assert_eq!(get_inbox(d.path(), "engineer-1", true).unwrap().len(), 0); // read once
        assert_eq!(get_inbox(d.path(), "engineer-1", false).unwrap().len(), 1); // full read still shows
        assert_eq!(get_inbox(d.path(), "nobody", false).unwrap().len(), 0);
    }

    #[test]
    fn test_list_messages_is_pure_read() {
        let d = tmp();
        set_liveness(d.path(), "b", Liveness::Alive).unwrap();
        send_message(d.path(), "a", "b", "hi", Category::Ticket).unwrap();
        assert_eq!(list_messages(d.path()).unwrap().len(), 1);
        assert_eq!(get_inbox(d.path(), "b", true).unwrap().len(), 1, "list must not consume");
    }
}
