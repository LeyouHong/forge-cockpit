//! A minimal MCP server (stdio) exposing the workspace request operations as
//! tools an agent can call: create_request, claim_request, get_request,
//! list_requests, submit_engineer_work, submit_review, submit_qa.
//!
//! MCP-over-stdio is just newline-delimited JSON-RPC 2.0. We hand-roll it (no
//! rmcp) so the whole protocol is visible and the dependency surface is tiny —
//! `initialize` → `tools/list` → `tools/call`.
//!
//! Root workspace dir: $FORGE_WORKSPACE_DIR (default ./.forge-workspace).
//! Wire it into Forge's .mcp.json:
//!   { "command": "forge-workspace-mcp",
//!     "env": { "FORGE_WORKSPACE_DIR": "/abs/path/.forge-workspace" } }

use std::io::{BufRead, Write};
use std::path::PathBuf;

use forge_workspace::message::{self, Category};
use forge_workspace::request::{
    self, Finding, NewRequest, QaResult, ReviewResult, Section, Severity,
};
use forge_workspace::RequestStatus;
use serde_json::{json, Value};

fn root() -> PathBuf {
    std::env::var("FORGE_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".forge-workspace"))
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        // Notifications (no id) get no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "forge-workspace", "version": env!("CARGO_PKG_VERSION") }
            })),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => handle_call(&msg),
            "ping" => Ok(json!({})),
            other => Err(format!("unknown method: {other}")),
        };

        let response = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}

/// Wrap a text result the way MCP `tools/call` expects.
fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn handle_call(msg: &Value) -> Result<Value, String> {
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let r = root();

    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let arr = |k: &str| -> Vec<String> {
        args.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };

    // Each arm returns Ok(text_result(...)) — tool errors are reported via
    // isError=true content, not JSON-RPC errors (so the agent can read them).
    let out = match name {
        "create_request" => match request::create_request(
            &r,
            NewRequest {
                title: s("title"),
                description: s("description"),
                acceptance_criteria: arr("acceptance_criteria"),
                batch: None,
            },
        ) {
            Ok(req) => text_result(format!("created {} (status: open)", req.id), false),
            Err(e) => text_result(format!("error: {e:#}"), true),
        },
        "claim_request" => match request::claim_request(&r, &s("id"), &s("agent")) {
            Ok(req) => text_result(format!("claimed {} (status: {:?})", req.id, req.status), false),
            Err(e) => text_result(format!("error: {e:#}"), true),
        },
        "get_request" => match request::get_request(&r, &s("id")) {
            Ok(Some((req, res))) => {
                let mut t = serde_yml::to_string(&req).unwrap_or_default();
                if let Some(res) = res {
                    t.push_str("--- response ---\n");
                    t.push_str(&serde_yml::to_string(&res).unwrap_or_default());
                }
                text_result(t, false)
            }
            Ok(None) => text_result(format!("no such request: {}", s("id")), true),
            Err(e) => text_result(format!("error: {e:#}"), true),
        },
        "list_requests" => {
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .and_then(|v| serde_yml::from_str::<RequestStatus>(v).ok());
            match request::list_requests(&r, status) {
                Ok(list) => {
                    let lines: Vec<String> = list
                        .iter()
                        .map(|q| {
                            format!(
                                "{}  [{:?}]  {}{}",
                                q.id,
                                q.status,
                                q.title,
                                q.claimed_by.as_deref().map(|a| format!("  (@{a})")).unwrap_or_default()
                            )
                        })
                        .collect();
                    text_result(if lines.is_empty() { "(no requests)".into() } else { lines.join("\n") }, false)
                }
                Err(e) => text_result(format!("error: {e:#}"), true),
            }
        }
        "submit_engineer_work" => submit(
            &r,
            &s("id"),
            Section::Engineer { files_changed: arr("files_changed"), notes: s("notes") },
        ),
        "submit_review" => {
            let result = match s("result").as_str() {
                "approved" => ReviewResult::Approved,
                "rejected" => ReviewResult::Rejected,
                _ => ReviewResult::ChangesRequested,
            };
            let findings = parse_findings(args.get("findings"));
            submit(&r, &s("id"), Section::Review { result, findings })
        }
        "submit_qa" => {
            let result = if s("result") == "passed" { QaResult::Passed } else { QaResult::Failed };
            submit(&r, &s("id"), Section::Qa { result, notes: s("notes") })
        }
        "send_message" => {
            let category = if s("category") == "notification" { Category::Notification } else { Category::Ticket };
            match message::send_message(&r, &s("from"), &s("to"), &s("body"), category) {
                Ok(m) => text_result(format!("sent {} to {}", m.id, m.to), false),
                Err(e) => text_result(format!("error: {e:#}"), true),
            }
        }
        "get_inbox" => {
            let unread_only = args.get("unread_only").and_then(Value::as_bool).unwrap_or(true);
            match message::get_inbox(&r, &s("agent"), unread_only) {
                Ok(msgs) => {
                    let text = if msgs.is_empty() {
                        "(inbox empty)".to_string()
                    } else {
                        msgs.iter()
                            .map(|m| format!("[{:?}] from {}: {}", m.category, m.from, m.body))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    text_result(text, false)
                }
                Err(e) => text_result(format!("error: {e:#}"), true),
            }
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    Ok(out)
}

fn submit(root: &std::path::Path, id: &str, section: Section) -> Value {
    match request::update_response(root, id, section) {
        Ok(req) => text_result(format!("{} -> status: {:?}", req.id, req.status), false),
        Err(e) => text_result(format!("error: {e:#}"), true),
    }
}

fn parse_findings(v: Option<&Value>) -> Vec<Finding> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|f| Finding {
                    severity: match f.get("severity").and_then(Value::as_str) {
                        Some("critical") => Severity::Critical,
                        Some("major") => Severity::Major,
                        _ => Severity::Minor,
                    },
                    file: f.get("file").and_then(Value::as_str).unwrap_or("").to_string(),
                    description: f.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    suggestion: f.get("suggestion").and_then(Value::as_str).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Tool definitions (name + description + JSON-Schema for arguments).
fn tool_defs() -> Value {
    let str_prop = |desc: &str| json!({ "type": "string", "description": desc });
    let str_arr = |desc: &str| json!({ "type": "array", "items": { "type": "string" }, "description": desc });
    let obj = |props: Value, req: Value| json!({ "type": "object", "properties": props, "required": req });

    json!([
        { "name": "create_request", "description": "Create a new work request (status: open) that agents will pick up.",
          "inputSchema": obj(json!({
            "title": str_prop("Short title"),
            "description": str_prop("Full description of the work"),
            "acceptance_criteria": str_arr("Testable acceptance criteria")
          }), json!(["title"])) },
        { "name": "claim_request", "description": "Claim an open request so no other agent works it. Sets status in_progress.",
          "inputSchema": obj(json!({ "id": str_prop("Request id"), "agent": str_prop("Your agent name") }), json!(["id","agent"])) },
        { "name": "get_request", "description": "Read a request and its response document (all sections written so far).",
          "inputSchema": obj(json!({ "id": str_prop("Request id") }), json!(["id"])) },
        { "name": "list_requests", "description": "List requests, optionally filtered by status (open/in_progress/review/qa/done/rejected).",
          "inputSchema": obj(json!({ "status": str_prop("Optional status filter") }), json!([])) },
        { "name": "submit_engineer_work", "description": "Engineer: record the implementation. Auto-advances the request to review.",
          "inputSchema": obj(json!({ "id": str_prop("Request id"), "files_changed": str_arr("Files you changed"), "notes": str_prop("Implementation notes") }), json!(["id"])) },
        { "name": "submit_review", "description": "Reviewer: record the review verdict + findings. approved->qa, changes_requested->back to engineer, rejected->rejected.",
          "inputSchema": obj(json!({
            "id": str_prop("Request id"),
            "result": json!({ "type": "string", "enum": ["approved","changes_requested","rejected"] }),
            "findings": json!({ "type": "array", "description": "Issues found",
              "items": { "type": "object", "properties": {
                "severity": { "type": "string", "enum": ["critical","major","minor"] },
                "file": { "type": "string" }, "description": { "type": "string" }, "suggestion": { "type": "string" } } } })
          }), json!(["id","result"])) },
        { "name": "submit_qa", "description": "QA: record the test verdict. passed->done, failed->back to engineer.",
          "inputSchema": obj(json!({ "id": str_prop("Request id"), "result": json!({ "type": "string", "enum": ["passed","failed"] }), "notes": str_prop("Test notes") }), json!(["id","result"])) },
        { "name": "send_message", "description": "Send a message to another agent (for rework details, help, coordination). Agents never talk directly.",
          "inputSchema": obj(json!({
            "from": str_prop("Your agent name"),
            "to": str_prop("Recipient agent name (e.g. engineer-1)"),
            "body": str_prop("Message text"),
            "category": json!({ "type": "string", "enum": ["notification","ticket"], "description": "notification = FYI; ticket = please act on it (default ticket)" })
          }), json!(["from","to","body"])) },
        { "name": "get_inbox", "description": "Read your inbox. Returned messages are marked read, so poll unread_only to see each once.",
          "inputSchema": obj(json!({ "agent": str_prop("Your agent name"), "unread_only": json!({ "type": "boolean", "description": "Only unread messages (default true)" }) }), json!(["agent"])) }
    ])
}
