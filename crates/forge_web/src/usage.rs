//! Usage analytics — token/cost tracking, aligned with the reference forge.
//!
//! Scans Claude Code session logs (`~/.claude/projects/*/*.jsonl`) — the same
//! source both products read — sums input/output/cache tokens per day, model,
//! and project, and estimates cost from a built-in price table. Pure on-demand
//! read; nothing is persisted.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::Json;
use forge_api::API;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{AppError, AppState};

/// USD per 1M tokens: (input, output, cache_read, cache_write). Matched by a
/// substring of the model id; unknown models fall back to a Sonnet-ish rate so
/// totals aren't zero.
fn price(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("opus") {
        (15.0, 75.0, 1.5, 18.75)
    } else if m.contains("haiku") {
        (0.80, 4.0, 0.08, 1.0)
    } else if m.contains("fable") || m.contains("mythos") {
        (5.0, 25.0, 0.5, 6.25)
    } else {
        // sonnet + unknown
        (3.0, 15.0, 0.30, 3.75)
    }
}

#[derive(Default, Clone)]
struct Agg {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    messages: u64,
    sessions: std::collections::HashSet<String>,
}

impl Agg {
    fn add(&mut self, other: &Row) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost += other.cost;
        self.messages += 1;
        self.sessions.insert(other.session.clone());
    }
    fn to_json(&self) -> Value {
        json!({
            "input": self.input, "output": self.output,
            "cache_read": self.cache_read, "cache_write": self.cache_write,
            "cost": (self.cost * 10000.0).round() / 10000.0,
            "messages": self.messages, "sessions": self.sessions.len(),
        })
    }
}

struct Row {
    day: String,
    project: String,
    model: String,
    session: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

fn claude_projects_dir() -> PathBuf {
    forge_workspace::pipeline::home_dir().join(".claude").join("projects")
}

/// Decode `~/.claude/projects` directory names (cwd with `/`→`-`) into a short
/// project label (the last path segment).
fn project_label(dir_name: &str) -> String {
    dir_name.rsplit('-').next().filter(|s| !s.is_empty()).unwrap_or(dir_name).to_string()
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse one JSONL line into a usage Row, if it carries assistant usage.
fn parse_row(line: &str, project: &str) -> Option<Row> {
    let o: Value = serde_json::from_str(line).ok()?;
    let msg = o.get("message")?;
    let usage = msg.get("usage").or_else(|| o.get("usage"))?;
    let output = u(usage, "output_tokens");
    let input = u(usage, "input_tokens");
    let cache_read = u(usage, "cache_read_input_tokens");
    let cache_write = u(usage, "cache_creation_input_tokens");
    if input + output + cache_read + cache_write == 0 {
        return None;
    }
    let model = msg
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "<synthetic>")
        .unwrap_or("unknown")
        .to_string();
    let day = o
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|t| t.get(..10).unwrap_or(t).to_string())
        .unwrap_or_default();
    let session = o.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let (pi, po, pr, pw) = price(&model);
    let cost = input as f64 / 1e6 * pi
        + output as f64 / 1e6 * po
        + cache_read as f64 / 1e6 * pr
        + cache_write as f64 / 1e6 * pw;
    Some(Row { day, project: project.to_string(), model, session, input, output, cache_read, cache_write, cost })
}

#[derive(Deserialize)]
pub(crate) struct UsageQuery {
    /// Days back from today; 0 or missing = all time.
    #[serde(default)]
    days: u64,
}

/// GET /api/usage?days=30 — aggregated token/cost analytics.
pub(crate) async fn usage<A: API>(
    State(_): State<AppState<A>>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Value>, AppError> {
    let cutoff = if q.days == 0 {
        String::new()
    } else {
        (chrono::Utc::now() - chrono::Duration::days(q.days as i64)).format("%Y-%m-%d").to_string()
    };

    let mut total = Agg::default();
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_project: BTreeMap<String, Agg> = BTreeMap::new();

    if let Ok(dirs) = std::fs::read_dir(claude_projects_dir()) {
        for d in dirs.flatten() {
            if !d.path().is_dir() {
                continue;
            }
            let project = project_label(&d.file_name().to_string_lossy());
            let Ok(files) = std::fs::read_dir(d.path()) else { continue };
            for f in files.flatten() {
                if f.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(f.path()) else { continue };
                for line in content.lines() {
                    let Some(row) = parse_row(line, &project) else { continue };
                    if !cutoff.is_empty() && row.day.as_str() < cutoff.as_str() {
                        continue;
                    }
                    total.add(&row);
                    *by_day.entry(row.day.clone()).or_default() += row.cost;
                    by_model.entry(row.model.clone()).or_default().add(&row);
                    by_project.entry(row.project.clone()).or_default().add(&row);
                }
            }
        }
    }

    let trend: Vec<Value> = by_day
        .iter()
        .map(|(d, c)| json!({ "day": d, "cost": (c * 10000.0).round() / 10000.0 }))
        .collect();
    let mut models: Vec<Value> = by_model
        .iter()
        .map(|(m, a)| {
            let mut v = a.to_json();
            v["model"] = json!(m);
            v
        })
        .collect();
    models.sort_by(|a, b| b["cost"].as_f64().partial_cmp(&a["cost"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    let mut projects: Vec<Value> = by_project
        .iter()
        .map(|(p, a)| {
            let mut v = a.to_json();
            v["project"] = json!(p);
            v
        })
        .collect();
    projects.sort_by(|a, b| b["cost"].as_f64().partial_cmp(&a["cost"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    projects.truncate(20);

    Ok(Json(json!({
        "total": total.to_json(),
        "days_with_activity": by_day.len(),
        "trend": trend,
        "by_model": models,
        "by_project": projects,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_row_and_cost() {
        let line = r#"{"timestamp":"2026-07-11T20:26:17.388Z","sessionId":"s1","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1000,"output_tokens":2000,"cache_read_input_tokens":500,"cache_creation_input_tokens":100}}}"#;
        let r = parse_row(line, "proj").unwrap();
        assert_eq!(r.day, "2026-07-11");
        assert_eq!(r.input, 1000);
        assert_eq!(r.output, 2000);
        // opus: 1000/1e6*15 + 2000/1e6*75 + 500/1e6*1.5 + 100/1e6*18.75
        let expect = 0.015 + 0.15 + 0.00075 + 0.001875;
        assert!((r.cost - expect).abs() < 1e-9, "cost {} vs {}", r.cost, expect);
    }

    #[test]
    fn test_parse_row_skips_empty_and_nonusage() {
        assert!(parse_row("not json", "p").is_none());
        assert!(parse_row(r#"{"type":"user","message":{"content":"hi"}}"#, "p").is_none());
        assert!(parse_row(r#"{"message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#, "p").is_none());
    }
}
