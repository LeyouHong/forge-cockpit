//! Serializable data-transfer objects for the web UI.
//!
//! [`ChatResponse`] is intentionally not `Serialize` in `forge_domain` (it
//! carries non-serializable runtime state such as `Arc<Notify>` in
//! `ToolCallStart`). This module maps it onto a flat, JSON-friendly enum that
//! the browser can consume over SSE.

use forge_domain::{ChatResponse, ChatResponseContent, ContextMessage, MessageEntry, Role};
use serde::Serialize;

/// A stored conversation message, flattened for history replay in the browser.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageDto {
    /// A text message from the system, user, or assistant.
    Text {
        role: String,
        content: String,
        /// Names of tool calls attached to an assistant message.
        tool_calls: Vec<String>,
    },
    /// The result of a tool invocation.
    Tool { name: String, output: String },
    /// An image attachment (content omitted in history view).
    Image,
}

impl MessageDto {
    /// Flattens stored context messages into a replay-friendly list.
    pub fn from_entries(entries: &[MessageEntry]) -> Vec<MessageDto> {
        entries
            .iter()
            .map(|entry| match &entry.message {
                ContextMessage::Text(text) => MessageDto::Text {
                    role: role_str(text.role).to_string(),
                    // User content is template-wrapped (`<task>…</task>` plus
                    // injected `<system_date>` etc). Show the original input:
                    // prefer the pre-template raw prompt, else strip the wrapper.
                    content: if text.role == Role::User {
                        text.raw_content
                            .as_ref()
                            .and_then(|v| v.as_user_prompt())
                            .map(|p| p.as_str().to_string())
                            .unwrap_or_else(|| strip_wrapper(&text.content))
                    } else {
                        text.content.clone()
                    },
                    tool_calls: text
                        .tool_calls
                        .as_ref()
                        .map(|calls| calls.iter().map(|c| c.name.as_str().to_string()).collect())
                        .unwrap_or_default(),
                },
                ContextMessage::Tool(result) => MessageDto::Tool {
                    name: result.name.as_str().to_string(),
                    output: result.output.as_str().unwrap_or_default().to_string(),
                },
                ContextMessage::Image(_) => MessageDto::Image,
            })
            .collect()
    }
}

/// Strips a leading `<tag>…</tag>` wrapper (e.g. `<task>` or `<feedback>`) from
/// rendered user content, returning just the inner text. Used only as a
/// fallback when the pre-template raw prompt is unavailable.
fn strip_wrapper(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix('<') {
        if let Some(open_end) = rest.find('>') {
            let tag = &rest[..open_end];
            let close = format!("</{tag}>");
            if let Some(close_pos) = t.find(&close) {
                let open_len = 1 + open_end + 1; // '<' + tag + '>'
                return t[open_len..close_pos].trim().to_string();
            }
        }
    }
    t.to_string()
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// A single streamed chat event, tagged by `type` for the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEventDto {
    /// Assistant markdown output. `partial` is true for streaming chunks.
    Text { text: String, partial: bool },
    /// Assistant reasoning / thinking output.
    Reasoning { text: String },
    /// A titled status line emitted before a tool runs.
    ToolInput {
        title: String,
        subtitle: Option<String>,
    },
    /// Raw output produced by a tool.
    ToolOutput { text: String },
    /// A tool call has started, with its (JSON) arguments.
    ToolCallStart { name: String, arguments: String },
    /// A tool call has finished, with its output.
    ToolCallEnd { name: String, output: String },
    /// A retry is being attempted after a failure.
    Retry { cause: String },
    /// The turn was interrupted (e.g. limits reached).
    Interrupt { reason: String },
    /// The turn finished successfully.
    Complete,
    /// A terminal error occurred while producing the stream.
    Error { message: String },
}

impl From<&ChatResponse> for ChatEventDto {
    fn from(resp: &ChatResponse) -> Self {
        match resp {
            ChatResponse::TaskMessage { content } => match content {
                ChatResponseContent::Markdown { text, partial } => {
                    ChatEventDto::Text { text: text.clone(), partial: *partial }
                }
                ChatResponseContent::ToolOutput(text) => {
                    ChatEventDto::ToolOutput { text: text.clone() }
                }
                ChatResponseContent::ToolInput(title) => ChatEventDto::ToolInput {
                    title: title.title.clone(),
                    subtitle: title.sub_title.clone(),
                },
            },
            ChatResponse::TaskReasoning { content } => {
                ChatEventDto::Reasoning { text: content.clone() }
            }
            ChatResponse::TaskComplete => ChatEventDto::Complete,
            ChatResponse::ToolCallStart { tool_call, .. } => ChatEventDto::ToolCallStart {
                name: tool_call.name.as_str().to_string(),
                arguments: tool_call.arguments.clone().into_string(),
            },
            ChatResponse::ToolCallEnd(result) => {
                ChatEventDto::ToolCallEnd {
                    name: result.name.as_str().to_string(),
                    output: result.output.as_str().unwrap_or_default().to_string(),
                }
            }
            ChatResponse::RetryAttempt { cause, .. } => {
                ChatEventDto::Retry { cause: cause.as_str().to_string() }
            }
            ChatResponse::Interrupt { reason } => {
                ChatEventDto::Interrupt { reason: format!("{reason:?}") }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_domain::ChatResponseContent;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_markdown_maps_to_text() {
        let resp = ChatResponse::TaskMessage {
            content: ChatResponseContent::Markdown {
                text: "hello".to_string(),
                partial: true,
            },
        };
        let dto = ChatEventDto::from(&resp);
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["partial"], true);
    }

    #[test]
    fn test_complete_maps_to_complete() {
        let dto = ChatEventDto::from(&ChatResponse::TaskComplete);
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["type"], "complete");
    }
}
