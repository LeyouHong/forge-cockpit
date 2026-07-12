//! Request documents + the status state machine.
//!
//! A *request* is the shared "blackboard" a task lives on. It is two YAML files
//! in `<root>/<id>/`:
//!   - `request.yml`  — what to do (created by Lead/Architect)
//!   - `response.yml` — results, written section-by-section by each role
//!
//! Writing a response section auto-advances the request status, which is how
//! work hands off from one role to the next without a central scheduler:
//!   engineer      → review
//!   review approved   → qa       (changes_requested → in_progress; rejected → rejected)
//!   qa passed         → done     (failed → in_progress)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Generates a short, human-ish request id like `req-a1b2c3d4`.
fn new_id() -> String {
    let raw = forge_domain::ConversationId::generate().into_string();
    let short: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("req-{short}")
}

// ---------------------------------------------------------------------------
// Status state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Open,
    InProgress,
    Review,
    Qa,
    Done,
    Rejected,
}

// ---------------------------------------------------------------------------
// request.yml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDocument {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub status: RequestStatus,
    pub created_at: String,
    /// Which agent claimed it (set by `claim_request`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Optional grouping for multi-module work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
}

/// Fields a caller supplies to create a request.
#[derive(Debug, Clone, Default)]
pub struct NewRequest {
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub batch: Option<String>,
}

// ---------------------------------------------------------------------------
// response.yml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseDocument {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engineer: Option<EngineerResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineerResponse {
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub notes: String,
    pub completed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewResult {
    Approved,
    ChangesRequested,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Major,
    Minor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default)]
    pub file: String,
    pub description: String,
    #[serde(default)]
    pub suggestion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResponse {
    pub result: ReviewResult,
    #[serde(default)]
    pub findings: Vec<Finding>,
    pub completed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaResult {
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaResponse {
    pub result: QaResult,
    #[serde(default)]
    pub notes: String,
    pub completed_at: String,
}

/// A response section a role writes back. Drives the state machine.
#[derive(Debug, Clone)]
pub enum Section {
    Engineer { files_changed: Vec<String>, notes: String },
    Review { result: ReviewResult, findings: Vec<Finding> },
    Qa { result: QaResult, notes: String },
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

fn dir_of(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}
fn request_path(root: &Path, id: &str) -> PathBuf {
    dir_of(root, id).join("request.yml")
}
fn response_path(root: &Path, id: &str) -> PathBuf {
    dir_of(root, id).join("response.yml")
}

fn write_request(root: &Path, req: &RequestDocument) -> Result<()> {
    std::fs::create_dir_all(dir_of(root, &req.id))?;
    let yaml = serde_yml::to_string(req).context("serialize request")?;
    std::fs::write(request_path(root, &req.id), yaml).context("write request.yml")?;
    Ok(())
}

fn write_response(root: &Path, res: &ResponseDocument) -> Result<()> {
    std::fs::create_dir_all(dir_of(root, &res.request_id))?;
    let yaml = serde_yml::to_string(res).context("serialize response")?;
    std::fs::write(response_path(root, &res.request_id), yaml).context("write response.yml")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API (the workspace MCP will wrap these)
// ---------------------------------------------------------------------------

/// Auto-notification (the reference forge's DAG broadcast): whenever a document
/// operation moves a request, the board notifies the members whose stage now
/// owns it — agents never hand-send these. Best-effort; a failed notification
/// must not fail the document operation.
fn notify_stage(root: &Path, req: &RequestDocument, note: &str) {
    use crate::team::Stage;
    let team = crate::team::load_team(root);
    let body = format!(
        "{note} — request `{}` (\"{}\") is now [{:?}].",
        req.id, req.title, req.status
    );
    // Terminal states (Done / Rejected) are FYI only — limit to the coordinator
    // so the whole plan-stage trio (PM, Architect, Coordinator) isn't pinged.
    if matches!(req.status, RequestStatus::Done | RequestStatus::Rejected) {
        if let Some(coord) = team.members.iter().find(|m| m.id == "coordinator") {
            let _ = crate::message::send_message(
                root,
                "board",
                &format!("{}-1", coord.id),
                &body,
                crate::message::Category::Notification,
            );
        }
        return;
    }
    let stage = match req.status {
        RequestStatus::Open | RequestStatus::InProgress => Stage::Implement,
        RequestStatus::Review => Stage::Review,
        RequestStatus::Qa => Stage::Qa,
        // Terminal states handled above; this arm is unreachable.
        RequestStatus::Done | RequestStatus::Rejected => Stage::Plan,
    };
    for m in team.members.iter().filter(|m| m.stage == stage) {
        let _ = crate::message::send_message(
            root,
            "board",
            &format!("{}-1", m.id),
            &body,
            crate::message::Category::Notification,
        );
    }
}

/// Create a new request in `status: open`. Returns the stored document.
pub fn create_request(root: &Path, input: NewRequest) -> Result<RequestDocument> {
    let req = RequestDocument {
        id: new_id(),
        title: input.title,
        description: input.description,
        acceptance_criteria: input.acceptance_criteria,
        status: RequestStatus::Open,
        created_at: now(),
        claimed_by: None,
        batch: input.batch,
    };
    write_request(root, &req)?;
    notify_stage(root, &req, "New request on the board — ready for your stage");
    Ok(req)
}

/// Read a request and its response (if any).
pub fn get_request(
    root: &Path,
    id: &str,
) -> Result<Option<(RequestDocument, Option<ResponseDocument>)>> {
    let rp = request_path(root, id);
    if !rp.exists() {
        return Ok(None);
    }
    let req: RequestDocument =
        serde_yml::from_str(&std::fs::read_to_string(rp)?).context("parse request.yml")?;
    let res = {
        let sp = response_path(root, id);
        if sp.exists() {
            Some(serde_yml::from_str(&std::fs::read_to_string(sp)?).context("parse response.yml")?)
        } else {
            None
        }
    };
    Ok(Some((req, res)))
}

/// List requests, newest first, optionally filtered by status.
pub fn list_requests(root: &Path, status: Option<RequestStatus>) -> Result<Vec<RequestDocument>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let rp = entry.path().join("request.yml");
        if let Ok(text) = std::fs::read_to_string(&rp)
            && let Ok(req) = serde_yml::from_str::<RequestDocument>(&text)
        {
            if status.is_none() || Some(req.status) == status {
                out.push(req);
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Claim an `open` request: sets status `in_progress` and records the agent.
/// Fails if already claimed (the anti-double-work lock).
pub fn claim_request(root: &Path, id: &str, agent: &str) -> Result<RequestDocument> {
    let (mut req, _) = get_request(root, id)?.context("no such request")?;
    if req.claimed_by.is_some() {
        anyhow::bail!("request {id} already claimed by {}", req.claimed_by.unwrap());
    }
    req.claimed_by = Some(agent.to_string());
    if req.status == RequestStatus::Open {
        req.status = RequestStatus::InProgress;
    }
    write_request(root, &req)?;
    Ok(req)
}

/// Write a response section and auto-advance the request status.
/// This is the heart of the hand-off: no scheduler, the transition drives flow.
pub fn update_response(root: &Path, id: &str, section: Section) -> Result<RequestDocument> {
    let (mut req, res) = get_request(root, id)?.context("no such request")?;
    let mut res = res.unwrap_or(ResponseDocument { request_id: id.to_string(), ..Default::default() });

    let section_kind = match &section {
        Section::Engineer { .. } => SectionKind::Engineer,
        Section::Review { .. } => SectionKind::Review,
        Section::Qa { .. } => SectionKind::Qa,
    };
    let next = match section {
        Section::Engineer { files_changed, notes } => {
            res.engineer = Some(EngineerResponse { files_changed, notes, completed_at: now() });
            RequestStatus::Review
        }
        Section::Review { result, findings } => {
            res.review = Some(ReviewResponse { result, findings, completed_at: now() });
            match result {
                ReviewResult::Approved => RequestStatus::Qa,
                ReviewResult::ChangesRequested => RequestStatus::InProgress,
                ReviewResult::Rejected => RequestStatus::Rejected,
            }
        }
        Section::Qa { result, notes } => {
            res.qa = Some(QaResponse { result, notes, completed_at: now() });
            match result {
                QaResult::Passed => RequestStatus::Done,
                QaResult::Failed => RequestStatus::InProgress,
            }
        }
    };

    req.status = next;
    write_response(root, &res)?;
    write_request(root, &req)?;
    let note = match (&req.status, &section_kind) {
        (RequestStatus::Review, _) => "Engineer work submitted — ready for your review",
        (RequestStatus::Qa, _) => "Review approved — ready for your QA",
        (RequestStatus::InProgress, SectionKind::Review) => {
            "Review requested changes — back to implementation"
        }
        (RequestStatus::InProgress, SectionKind::Qa) => "QA failed — back to implementation",
        (RequestStatus::Done, _) => "QA passed — request complete (FYI)",
        (RequestStatus::Rejected, _) => "Review rejected the request (FYI)",
        _ => "Request status changed",
    };
    notify_stage(root, &req, note);
    Ok(req)
}

/// Which section an update came from (for notification wording).
enum SectionKind {
    Engineer,
    Review,
    Qa,
}

// ---------------------------------------------------------------------------
// Tests — exercise the state machine with no agents involved.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn create_then_list_and_get() {
        let d = tmp();
        let req = create_request(
            d.path(),
            NewRequest { title: "rate-limit login".into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(req.status, RequestStatus::Open);
        let all = list_requests(d.path(), None).unwrap();
        assert_eq!(all.len(), 1);
        let (got, res) = get_request(d.path(), &req.id).unwrap().unwrap();
        assert_eq!(got.id, req.id);
        assert!(res.is_none());
    }

    #[test]
    fn claim_locks_out_second_agent() {
        let d = tmp();
        let req = create_request(d.path(), NewRequest { title: "x".into(), ..Default::default() }).unwrap();
        let claimed = claim_request(d.path(), &req.id, "eng-1").unwrap();
        assert_eq!(claimed.status, RequestStatus::InProgress);
        assert_eq!(claimed.claimed_by.as_deref(), Some("eng-1"));
        assert!(claim_request(d.path(), &req.id, "eng-2").is_err());
    }

    #[test]
    fn transitions_auto_notify_the_next_stage() {
        // Default team applies (no .team.json): engineer / reviewer / qa /
        // plan trio. Each transition should drop a Notification in the inbox
        // of the stage that now owns the request.
        let d = tmp();
        let unread = |who: &str| crate::message::get_inbox(d.path(), who, true).unwrap().len();

        let req = create_request(d.path(), NewRequest { title: "x".into(), ..Default::default() }).unwrap();
        assert_eq!(unread("engineer-1"), 1, "create → implementers notified");

        claim_request(d.path(), &req.id, "engineer-1").unwrap();
        update_response(
            d.path(),
            &req.id,
            Section::Engineer { files_changed: vec![], notes: "done".into() },
        )
        .unwrap();
        assert_eq!(unread("reviewer-1"), 1, "submit → reviewer notified");

        update_response(
            d.path(),
            &req.id,
            Section::Review { result: ReviewResult::ChangesRequested, findings: vec![] },
        )
        .unwrap();
        // get_inbox marks messages read on fetch, so this is the NEW unread one.
        assert_eq!(unread("engineer-1"), 1, "changes requested → engineer re-notified");

        update_response(
            d.path(),
            &req.id,
            Section::Engineer { files_changed: vec![], notes: "fixed".into() },
        )
        .unwrap();
        update_response(
            d.path(),
            &req.id,
            Section::Review { result: ReviewResult::Approved, findings: vec![] },
        )
        .unwrap();
        assert_eq!(unread("qa-1"), 1, "approved → qa notified");

        update_response(d.path(), &req.id, Section::Qa { result: QaResult::Passed, notes: "ok".into() }).unwrap();
        assert_eq!(unread("coordinator-1"), 1, "done → lead gets the FYI");
    }

    #[test]
    fn happy_path_flows_to_done() {
        let d = tmp();
        let req = create_request(d.path(), NewRequest { title: "x".into(), ..Default::default() }).unwrap();
        claim_request(d.path(), &req.id, "eng-1").unwrap();

        let r = update_response(
            d.path(),
            &req.id,
            Section::Engineer { files_changed: vec!["auth.rs".into()], notes: "done".into() },
        )
        .unwrap();
        assert_eq!(r.status, RequestStatus::Review); // engineer -> review

        let r = update_response(
            d.path(),
            &req.id,
            Section::Review { result: ReviewResult::Approved, findings: vec![] },
        )
        .unwrap();
        assert_eq!(r.status, RequestStatus::Qa); // approved -> qa

        let r = update_response(
            d.path(),
            &req.id,
            Section::Qa { result: QaResult::Passed, notes: "ok".into() },
        )
        .unwrap();
        assert_eq!(r.status, RequestStatus::Done); // passed -> done
    }

    #[test]
    fn changes_requested_bounces_back() {
        let d = tmp();
        let req = create_request(d.path(), NewRequest { title: "x".into(), ..Default::default() }).unwrap();
        update_response(
            d.path(),
            &req.id,
            Section::Engineer { files_changed: vec![], notes: "".into() },
        )
        .unwrap();
        let r = update_response(
            d.path(),
            &req.id,
            Section::Review {
                result: ReviewResult::ChangesRequested,
                findings: vec![Finding {
                    severity: Severity::Major,
                    file: "auth.rs".into(),
                    description: "missing lockout".into(),
                    suggestion: "add a counter".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(r.status, RequestStatus::InProgress); // back to engineer
    }
}
