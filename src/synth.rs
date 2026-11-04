//! Synthetic Claude Code sessions with planted ground truth (lightning-spec 14, "Testing").
//!
//! Each pair gets a success probability, a token scale and a re-read probability; the generator
//! writes transcripts in the real JSONL shape and returns the truth for every segment so tests can
//! check that a planted effect is found and a planted null is not.
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;

use crate::stats::Rng;

#[derive(Debug, Clone)]
pub struct PairSpec {
    pub model: String,
    pub success_p: f64,
    /// Multiplies every token count.
    pub token_scale: f64,
    /// Chance that a segment re-reads a file it already read (a "pair ignores rule X" effect).
    pub reread_p: f64,
}

impl PairSpec {
    pub fn new(model: &str, success_p: f64, token_scale: f64) -> Self {
        Self { model: model.into(), success_p, token_scale, reread_p: 0.05 }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub repos: Vec<String>,
    pub tasks: Vec<&'static str>,
    pub pairs: Vec<PairSpec>,
    /// Segments per (pair, task).
    pub segments: usize,
    pub seed: u64,
    /// First day (UTC) of generated activity.
    pub start: (i32, u32, u32),
    /// Claude Code version written on every line.
    pub version: String,
    /// Directory the repos live under (sessions record `<cwd_root>/<repo>` as their cwd).
    pub cwd_root: String,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            repos: vec!["alpha".into(), "beta".into()],
            tasks: vec!["debugging"],
            pairs: Vec::new(),
            segments: 40,
            seed: 7,
            start: (2026, 10, 1),
            version: "3.1.0".into(),
            cwd_root: "/work".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truth {
    pub session_id: String,
    pub model: String,
    pub task: &'static str,
    pub success: bool,
    pub reread: bool,
}

fn prompt_for(task: &str, i: usize) -> String {
    match task {
        "debugging" => format!("Fix the crash in the parser when input {i} is empty"),
        "refactor" => format!("Refactor the store module and rename helper {i}"),
        "feature" => format!("Add a new export command with option {i}"),
        "test_writing" => format!("Write unit tests for the report module case {i}"),
        "exploration" => format!("Explain how the ingest pipeline handles file {i}"),
        "config_devops" => format!("Set up the CI workflow config for target {i}"),
        _ => format!("Update the README section {i}"),
    }
}

/// Write transcripts under `root` (a Claude Code projects directory) and return the truth.
pub fn generate(root: &Path, plan: &Plan) -> Result<Vec<Truth>> {
    let mut rng = Rng::new(plan.seed);
    let mut truth = Vec::new();
    let t0 = Utc.with_ymd_and_hms(plan.start.0, plan.start.1, plan.start.2, 9, 0, 0).unwrap();
    let mut n = 0usize;
    for i in 0..plan.segments {
        for task in &plan.tasks {
            for pair in &plan.pairs {
                let repo = &plan.repos[(i + n) % plan.repos.len()];
                let cwd = format!("{}/{repo}", plan.cwd_root.trim_end_matches(['/', '\\']));
                let slug: String = cwd.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
                let sid = format!("{:08x}-0000-4000-8000-{:012x}", plan.seed as u32, n);
                let mut ts = t0 + Duration::hours((n / 4) as i64) + Duration::minutes((n % 4) as i64 * 10);
                let success = (rng.next_u64() % 10_000) as f64 / 10_000.0 < pair.success_p;
                let reread = (rng.next_u64() % 10_000) as f64 / 10_000.0 < pair.reread_p;
                let mut text = String::new();
                let base = json!({"cwd": cwd, "sessionId": sid, "version": plan.version, "gitBranch": "main"});
                let mut line = |v: serde_json::Value| {
                    let mut o = base.clone();
                    for (k, val) in v.as_object().unwrap() {
                        o[k] = val.clone();
                    }
                    writeln!(text, "{o}").unwrap();
                };
                let stamp = |t: chrono::DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                line(json!({"type": "user", "uuid": format!("p{n}"), "timestamp": stamp(ts), "message": {"role": "user", "content": prompt_for(task, i)}}));
                let file = match *task {
                    "test_writing" => format!("{cwd}/tests/case_{i}.rs"),
                    "docs" => format!("{cwd}/README.md"),
                    "config_devops" => format!("{cwd}/.github/workflows/ci.yml"),
                    _ => format!("{cwd}/src/mod_{i}.rs"),
                };
                let tools: Vec<(&str, serde_json::Value, bool)> = {
                    let mut v = vec![("Read", json!({"file_path": file}), false)];
                    if reread {
                        v.push(("Read", json!({"file_path": file}), false));
                    }
                    if *task != "exploration" {
                        v.push(("Edit", json!({"file_path": file, "old_string": "a", "new_string": "b"}), false));
                        v.push(("Bash", json!({"command": "cargo test"}), !success));
                    }
                    v
                };
                let scale = pair.token_scale;
                for (k, (name, input, is_error)) in tools.iter().enumerate() {
                    ts += Duration::seconds(20);
                    let mid = format!("msg_{n}_{k}");
                    let tid = format!("toolu_{n}_{k}");
                    let usage = json!({
                        "input_tokens": (40.0 * scale) as i64,
                        "output_tokens": (300.0 * scale) as i64,
                        "cache_read_input_tokens": (20_000.0 * scale) as i64,
                        "cache_creation_input_tokens": (2_000.0 * scale) as i64
                    });
                    line(json!({"type": "assistant", "uuid": format!("a{n}_{k}"), "timestamp": stamp(ts), "effort": "medium",
                        "message": {"id": mid, "model": pair.model, "role": "assistant", "stop_reason": "tool_use",
                            "content": [{"type": "tool_use", "id": tid, "name": name, "input": input}], "usage": usage}}));
                    ts += Duration::seconds(5);
                    line(json!({"type": "user", "uuid": format!("r{n}_{k}"), "timestamp": stamp(ts),
                        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid, "is_error": is_error, "content": "x"}]}}));
                }
                ts += Duration::seconds(20);
                line(json!({"type": "assistant", "uuid": format!("a{n}_end"), "timestamp": stamp(ts),
                    "message": {"id": format!("msg_{n}_end"), "model": pair.model, "role": "assistant", "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "done"}],
                        "usage": {"input_tokens": (10.0 * scale) as i64, "output_tokens": (100.0 * scale) as i64, "cache_read_input_tokens": (21_000.0 * scale) as i64}}}));
                // The user moves on after success and pushes back after failure.
                ts += Duration::minutes(2);
                let follow = if success { "yes, thanks" } else { "no, that's still broken, try again" };
                line(json!({"type": "user", "uuid": format!("f{n}"), "timestamp": stamp(ts), "message": {"role": "user", "content": follow}}));
                let dir = root.join(&slug);
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(format!("{sid}.jsonl")), text)?;
                truth.push(Truth { session_id: sid, model: pair.model.clone(), task, success, reread });
                n += 1;
            }
        }
    }
    Ok(truth)
}

/// A price table with made-up but consistent prices for the given models (for tests only).
pub fn test_pricing(models: &[&str]) -> crate::pricing::PriceTable {
    let mut t = String::from("version = \"2026-11-01\"\n");
    for m in models {
        writeln!(t, "[[model]]\nid = \"{m}\"\ninput_per_mtok = 3.0\noutput_per_mtok = 15.0\ncache_read_per_mtok = 0.3\ncache_write_5m_per_mtok = 3.75\ncache_write_1h_per_mtok = 6.0").unwrap();
    }
    crate::pricing::PriceTable::parse(&t).expect("test pricing parses")
}
