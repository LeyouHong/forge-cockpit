//! The message bus — inter-agent messaging for the exceptions the request state
//! machine doesn't cover: rework details, help requests, coordination.
//!
//! Messages are one YAML file each under `<root>/messages/`. Agents don't talk
//! directly; they drop a message and the recipient reads its inbox. A
//! `notification` is fire-and-forget; a `ticket` is a message you expect the
//! recipient to act on. Reading an inbox marks the returned messages read, so an
//! agent polling `unread_only` sees each message exactly once.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
    /// Fire-and-forget FYI.
    Notification,
    /// Expected to be acted on by the recipient.
    Ticket,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub category: Category,
    pub body: String,
    pub created_at: String,
    #[serde(default)]
    pub read: bool,
}

fn msg_dir(root: &Path) -> PathBuf {
    root.join("messages")
}
fn msg_path(root: &Path, id: &str) -> PathBuf {
    msg_dir(root).join(format!("{id}.yml"))
}

fn write_message(root: &Path, m: &Message) -> Result<()> {
    std::fs::create_dir_all(msg_dir(root))?;
    let yaml = serde_yml::to_string(m).context("serialize message")?;
    std::fs::write(msg_path(root, &m.id), yaml).context("write message")?;
    Ok(())
}

/// Send a message from one agent to another.
pub fn send_message(
    root: &Path,
    from: &str,
    to: &str,
    body: &str,
    category: Category,
) -> Result<Message> {
    let m = Message {
        id: new_id(),
        from: from.to_string(),
        to: to.to_string(),
        category,
        body: body.to_string(),
        created_at: now(),
        read: false,
    };
    write_message(root, &m)?;
    Ok(m)
}

/// Read `agent`'s inbox (optionally only unread), oldest first. Returned messages
/// are marked read so a subsequent `unread_only` poll won't see them again.
pub fn get_inbox(root: &Path, agent: &str, unread_only: bool) -> Result<Vec<Message>> {
    let dir = msg_dir(root);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(m) = serde_yml::from_str::<Message>(&text)
            && m.to == agent
            && (!unread_only || !m.read)
        {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    // Mark returned messages read.
    for m in out.iter_mut() {
        if !m.read {
            m.read = true;
            let _ = write_message(root, m);
        }
    }
    Ok(out)
}

/// List every message on the bus, newest first, WITHOUT marking anything read.
/// For dashboards / monitoring — unlike [`get_inbox`] this is a pure read.
pub fn list_messages(root: &Path) -> Result<Vec<Message>> {
    let dir = msg_dir(root);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(m) = serde_yml::from_str::<Message>(&text)
        {
            out.push(m);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at)); // newest first
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn list_messages_is_pure_read() {
        let d = tmp();
        send_message(d.path(), "a", "b", "hi", Category::Ticket).unwrap();
        send_message(d.path(), "a", "c", "yo", Category::Notification).unwrap();
        // lists everything regardless of recipient, newest first
        let all = list_messages(d.path()).unwrap();
        assert_eq!(all.len(), 2);
        // does NOT mark read — an unread inbox poll still sees the message
        assert_eq!(get_inbox(d.path(), "b", true).unwrap().len(), 1);
    }

    #[test]
    fn send_then_read_once() {
        let d = tmp();
        send_message(d.path(), "reviewer-1", "engineer-1", "please fix lockout", Category::Ticket).unwrap();
        let inbox = get_inbox(d.path(), "engineer-1", true).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, "reviewer-1");
        // second unread poll sees nothing (marked read)
        assert_eq!(get_inbox(d.path(), "engineer-1", true).unwrap().len(), 0);
        // but a full read still shows it
        assert_eq!(get_inbox(d.path(), "engineer-1", false).unwrap().len(), 1);
    }

    #[test]
    fn inbox_is_per_recipient() {
        let d = tmp();
        send_message(d.path(), "lead", "engineer-1", "a", Category::Notification).unwrap();
        send_message(d.path(), "lead", "qa-1", "b", Category::Notification).unwrap();
        assert_eq!(get_inbox(d.path(), "engineer-1", false).unwrap().len(), 1);
        assert_eq!(get_inbox(d.path(), "qa-1", false).unwrap().len(), 1);
        assert_eq!(get_inbox(d.path(), "nobody", false).unwrap().len(), 0);
    }
}
