//! Multi-agent orchestration for Forge — the *spine*.
//!
//! This crate provides the two foundational primitives every multi-agent
//! workflow rests on, with no dependency on agents or LLMs:
//!
//! - [`request`] — the shared **request documents** (blackboard) that tasks
//!   live on, and the **status state machine** that hands work off between
//!   roles (engineer → review → qa → done).
//! - [`message`] — the **message bus** for inter-agent messaging (the exceptions
//!   the state machine doesn't cover: rework details, help, coordination).
//!
//! Higher layers (a workspace MCP exposing these ops to agents, role SOPs, a
//! runner that drives agent sessions) build on top of this.

pub mod message;
pub mod request;

pub use message::{get_inbox, list_messages, send_message, Category, Message};
pub use request::{
    claim_request, create_request, get_request, list_requests, update_response, Finding, NewRequest,
    QaResult, RequestDocument, RequestStatus, ResponseDocument, ReviewResult, Section, Severity,
};
