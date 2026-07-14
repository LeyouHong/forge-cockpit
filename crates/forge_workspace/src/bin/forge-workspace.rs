//! Tiny CLI to drive the request spine by hand (before agents are wired up).
//!
//!   forge-workspace [--root DIR] <cmd> ...
//!     create   --title T [--criteria C]...      create a request (status: open)
//!     list     [--status S]                     list requests
//!     get      <id>                             show request + response
//!     claim    <id> <agent>                     claim (open -> in_progress)
//!     engineer <id> [--files a,b] [--notes N]   write engineer section (-> review)
//!     review   <id> --result R                  approved|changes_requested|rejected
//!     qa       <id> --result R                  passed|failed
//!     pause    <member>                         hold new work for a member
//!     resume   <member>                         release the hold
//!
//! Default root: ./.forge-workspace (override with --root or FORGE_WORKSPACE_DIR).

use std::path::PathBuf;

use forge_workspace::request::{self, NewRequest, QaResult, ReviewResult, Section};
use forge_workspace::RequestStatus;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Pulls `--flag value` (repeatable) out of args, returning the values and the
/// remaining positionals.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            out.push(args.remove(i + 1));
            args.remove(i);
        } else {
            i += 1;
        }
    }
    out
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let root = take_flag(&mut args, "--root")
        .into_iter()
        .next()
        .or_else(|| std::env::var("FORGE_WORKSPACE_DIR").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".forge-workspace"));

    let cmd = args.first().cloned().unwrap_or_default();
    let rest: Vec<String> = args.into_iter().skip(1).collect();

    match cmd.as_str() {
        "create" => {
            let mut a = rest;
            let title = take_flag(&mut a, "--title").into_iter().next().unwrap_or_default();
            let acceptance_criteria = take_flag(&mut a, "--criteria");
            let description = take_flag(&mut a, "--desc").into_iter().next().unwrap_or_default();
            let req = request::create_request(
                &root,
                NewRequest { title, description, acceptance_criteria, batch: None },
            )?;
            println!("created {} [{:?}]", req.id, req.status);
        }
        "list" => {
            let mut a = rest;
            let status = take_flag(&mut a, "--status")
                .into_iter()
                .next()
                .and_then(|s| serde_yml::from_str::<RequestStatus>(&s).ok());
            for r in request::list_requests(&root, status)? {
                let who = r.claimed_by.as_deref().unwrap_or("-");
                println!("{:<14} {:<14?} [{}]  {}", r.id, r.status, who, r.title);
            }
        }
        "get" => {
            let id = rest.first().cloned().unwrap_or_default();
            match request::get_request(&root, &id)? {
                Some((req, res)) => {
                    println!("{}", serde_yml::to_string(&req)?);
                    if let Some(res) = res {
                        println!("--- response ---\n{}", serde_yml::to_string(&res)?);
                    }
                }
                None => println!("no such request: {id}"),
            }
        }
        "claim" => {
            let id = rest.first().cloned().unwrap_or_default();
            let agent = rest.get(1).cloned().unwrap_or_else(|| "agent".into());
            let r = request::claim_request(&root, &id, &agent)?;
            println!("{} claimed by {} [{:?}]", r.id, agent, r.status);
        }
        "engineer" => {
            let mut a = rest;
            let id = a.first().cloned().unwrap_or_default();
            let files_changed = take_flag(&mut a, "--files")
                .into_iter()
                .next()
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default();
            let notes = take_flag(&mut a, "--notes").into_iter().next().unwrap_or_default();
            let r = request::update_response(&root, &id, Section::Engineer { files_changed, notes })?;
            println!("{} -> {:?}", r.id, r.status);
        }
        "review" => {
            let mut a = rest;
            let id = a.first().cloned().unwrap_or_default();
            let result = match take_flag(&mut a, "--result").into_iter().next().as_deref() {
                Some("approved") => ReviewResult::Approved,
                Some("rejected") => ReviewResult::Rejected,
                _ => ReviewResult::ChangesRequested,
            };
            let r = request::update_response(&root, &id, Section::Review { result, findings: vec![] })?;
            println!("{} -> {:?}", r.id, r.status);
        }
        "qa" => {
            let mut a = rest;
            let id = a.first().cloned().unwrap_or_default();
            let result = match take_flag(&mut a, "--result").into_iter().next().as_deref() {
                Some("passed") => QaResult::Passed,
                _ => QaResult::Failed,
            };
            let r = request::update_response(&root, &id, Section::Qa { result, notes: String::new() })?;
            println!("{} -> {:?}", r.id, r.status);
        }
        "pause" | "resume" => {
            let Some(member) = rest.first() else {
                anyhow::bail!("usage: forge-workspace pause|resume <member-id>");
            };
            let paused = cmd == "pause";
            forge_workspace::team::set_paused(&root, member, paused)?;
            println!(
                "{member} {}",
                if paused { "paused — finishes current work, takes nothing new" } else { "resumed" }
            );
        }
        other => {
            eprintln!("unknown command: {other:?}\nsee the header of this file for usage");
            std::process::exit(2);
        }
    }
    Ok(())
}
