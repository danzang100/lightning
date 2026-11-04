//! Hand labels and the go/no-go agreement check (lightning-spec 15.1, 15.2).
//!
//! The labeler sees a segment's opening prompt read live from the source transcript; the text is
//! printed, never stored. Labels hold only a task type and an outcome.
use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::classify::TASK_TYPES;
use crate::segment::Segment;
use crate::stats::Rng;
use crate::store::Store;

pub const TASK_THRESHOLD: f64 = 0.80;
pub const OUTCOME_THRESHOLD: f64 = 0.75;
pub const MIN_LABELS: u64 = 30;
pub const OUTCOMES: [&str; 3] = ["likely_success", "likely_failure", "unknown"];

pub fn set(store: &Store, segment_id: &str, task: Option<&str>, outcome: Option<&str>, now: DateTime<Utc>) -> Result<()> {
    if let Some(t) = task
        && !TASK_TYPES.contains(&t)
        && t != "unknown"
    {
        bail!("unknown task type {t:?} ({}, unknown)", TASK_TYPES.join(", "));
    }
    let outcome = outcome.map(|o| match o {
        "success" | "s" => "likely_success",
        "failure" | "f" => "likely_failure",
        "unknown" | "u" => "unknown",
        other => other,
    });
    if let Some(o) = outcome
        && !OUTCOMES.contains(&o)
    {
        bail!("unknown outcome {o:?} (success, failure, unknown)");
    }
    store.conn().execute(
        "INSERT INTO labels(segment_id, task_type, outcome, labeled_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(segment_id) DO UPDATE SET task_type = COALESCE(?2, task_type), outcome = COALESCE(?3, outcome), labeled_at = ?4",
        params![segment_id, task, outcome, now.to_rfc3339_opts(SecondsFormat::Secs, true)],
    )?;
    Ok(())
}

pub fn get(store: &Store, segment_id: &str) -> Result<Option<(Option<String>, Option<String>)>> {
    Ok(store
        .conn()
        .query_row("SELECT task_type, outcome FROM labels WHERE segment_id = ?1", params![segment_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// Unlabeled segments, sampled across pairs and task types so the labeled set is balanced.
pub fn sample<'a>(store: &Store, segs: &'a [Segment], n: usize, seed: u64) -> Result<Vec<&'a Segment>> {
    let mut pool: Vec<&Segment> = Vec::new();
    for s in segs {
        if get(store, &s.segment_id)?.is_none() {
            pool.push(s);
        }
    }
    let mut rng = Rng::new(seed);
    for i in (1..pool.len()).rev() {
        pool.swap(i, rng.below(i + 1));
    }
    // Round-robin over (pair, heuristic task) so no single bucket dominates.
    let mut buckets: std::collections::BTreeMap<String, Vec<&Segment>> = std::collections::BTreeMap::new();
    for s in pool {
        buckets.entry(format!("{}|{}", s.pair(), s.task_type)).or_default().push(s);
    }
    let mut out = Vec::new();
    while out.len() < n && buckets.values().any(|v| !v.is_empty()) {
        for v in buckets.values_mut() {
            if let Some(s) = v.pop() {
                out.push(s);
                if out.len() == n {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn excerpt(text: &str, max: usize) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max { format!("{}...", t.chars().take(max).collect::<String>()) } else { t }
}

/// The opening prompt of a segment, read from its source now (never stored).
pub fn prompt_text(store: &Store, s: &Segment) -> Result<Option<String>> {
    let (Some(fid), Some(r)) = (s.prompt_file_id, s.prompt_ref.as_deref()) else { return Ok(None) };
    let Some(path) = store.file_path(fid)? else { return Ok(None) };
    if let Some(n) = r.strip_prefix("line:") {
        let n: usize = n.parse()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        let Some(line) = text.lines().nth(n - 1) else { return Ok(None) };
        let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let content = v.get("message").and_then(|m| m.get("content")).or_else(|| v.get("payload").and_then(|p| p.get("message")));
        return Ok(match content {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => Some(a.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")),
            _ => None,
        });
    }
    if let Some(mid) = r.strip_prefix("message:") {
        let Ok(conn) = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return Ok(None) };
        let mut st = conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
        let parts: Vec<String> = st.query_map(params![mid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let text: Vec<String> = parts
            .iter()
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str).map(String::from))
            .collect();
        return Ok((!text.is_empty()).then(|| text.join("\n")));
    }
    Ok(None)
}

pub fn describe(store: &Store, s: &Segment) -> Result<String> {
    let prompt = prompt_text(store, s)?.map(|p| excerpt(&p, 400)).unwrap_or_else(|| "(source transcript no longer available)".into());
    Ok(format!(
        "segment {}  {}  repo {}  {}\n  pair {}  turns {}  edits {}  tool errors {}  corrections {}\n  heuristic: task {} ({:.0}%), outcome {} [{}]\n  prompt: {}\n",
        s.segment_id,
        s.start.as_deref().unwrap_or("?"),
        s.repo,
        s.session_id,
        s.pair(),
        s.turns,
        s.edits,
        s.tool_errors,
        s.corrections,
        s.task_type,
        s.task_confidence * 100.0,
        s.outcome,
        s.outcome_signals.join(", "),
        prompt
    ))
}

/// Interactive labeling over a sample. Returns how many segments were labeled.
pub fn interactive<R: BufRead, W: Write>(store: &Store, segs: &[Segment], n: usize, input: &mut R, out: &mut W, now: DateTime<Utc>) -> Result<usize> {
    let picked = sample(store, segs, n, now.timestamp() as u64)?;
    if picked.is_empty() {
        writeln!(out, "nothing to label")?;
        return Ok(0);
    }
    writeln!(out, "Label {} segment(s). Task: number or name, Enter keeps the heuristic, s skips, q quits.", picked.len())?;
    let opts: Vec<String> = TASK_TYPES.iter().enumerate().map(|(i, t)| format!("{}={t}", i + 1)).collect();
    writeln!(out, "Tasks: {}, 0=unknown. Outcome: s=success, f=failure, u=unknown.\n", opts.join(" "))?;
    let mut done = 0;
    let read = |input: &mut R| -> Result<String> {
        let mut l = String::new();
        input.read_line(&mut l)?;
        Ok(l.trim().to_string())
    };
    for s in picked {
        write!(out, "{}", describe(store, s)?)?;
        write!(out, "  task> ")?;
        out.flush()?;
        let t = read(input)?;
        if t == "q" {
            break;
        }
        if t == "s" {
            continue;
        }
        let task = match t.as_str() {
            "" => s.task_type.clone(),
            "0" => "unknown".to_string(),
            x => match x.parse::<usize>() {
                Ok(i) if (1..=TASK_TYPES.len()).contains(&i) => TASK_TYPES[i - 1].to_string(),
                _ => x.to_string(),
            },
        };
        write!(out, "  outcome> ")?;
        out.flush()?;
        let o = read(input)?;
        if o == "q" {
            break;
        }
        let outcome = match o.as_str() {
            "" => s.outcome.clone(),
            x => x.to_string(),
        };
        match set(store, &s.segment_id, Some(&task), Some(&outcome), now) {
            Ok(()) => done += 1,
            Err(e) => writeln!(out, "  skipped: {e}")?,
        }
        writeln!(out)?;
    }
    writeln!(out, "labeled {done}")?;
    Ok(done)
}

#[derive(Debug, Clone, Serialize)]
pub struct Agreement {
    pub labeled: u64,
    pub task_labeled: u64,
    pub task_agreement: Option<f64>,
    pub outcome_labeled: u64,
    /// Heuristic `unknown` counts as a miss when the human decided.
    pub outcome_agreement: Option<f64>,
    pub outcome_coverage: Option<f64>,
    pub task_threshold: f64,
    pub outcome_threshold: f64,
    pub min_labels: u64,
    pub verdict: String,
    pub notes: Vec<String>,
}

/// Agreement between heuristics and hand labels (the §15.2 gate).
pub fn agreement(store: &Store, segs: &[Segment]) -> Result<Agreement> {
    let by_id: std::collections::HashMap<&str, &Segment> = segs.iter().map(|s| (s.segment_id.as_str(), s)).collect();
    let mut st = store.conn().prepare("SELECT segment_id, task_type, outcome FROM labels")?;
    let rows: Vec<(String, Option<String>, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let (mut labeled, mut tl, mut ta, mut ol, mut oa, mut oc) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut missing = 0;
    for (id, task, outcome) in rows {
        let Some(s) = by_id.get(id.as_str()) else {
            missing += 1;
            continue;
        };
        labeled += 1;
        if let Some(t) = task.filter(|t| t != "unknown") {
            tl += 1;
            ta += (s.task_type == t) as u64;
        }
        if let Some(o) = outcome.filter(|o| o != "unknown") {
            ol += 1;
            oa += (s.outcome == o) as u64;
            oc += (s.outcome != "unknown") as u64;
        }
    }
    let r = |k: u64, n: u64| (n > 0).then(|| k as f64 / n as f64);
    let (task_agreement, outcome_agreement) = (r(ta, tl), r(oa, ol));
    let mut notes = Vec::new();
    if missing > 0 {
        notes.push(format!("{missing} label(s) point at segments that no longer exist (a heuristic changed segment boundaries)"));
    }
    let verdict = if tl.min(ol) < MIN_LABELS {
        notes.push(format!("label at least {MIN_LABELS} segments (30-50 recommended) before reading the verdict"));
        "insufficient_labels"
    } else if task_agreement.unwrap_or(0.0) >= TASK_THRESHOLD && outcome_agreement.unwrap_or(0.0) >= OUTCOME_THRESHOLD {
        "go"
    } else {
        "no_go"
    };
    if verdict == "no_go" {
        notes.push("below threshold: narrow to cost and waste diagnostics, or improve the heuristics, before trusting pair comparisons".into());
    }
    Ok(Agreement {
        labeled,
        task_labeled: tl,
        task_agreement,
        outcome_labeled: ol,
        outcome_agreement,
        outcome_coverage: r(oc, ol),
        task_threshold: TASK_THRESHOLD,
        outcome_threshold: OUTCOME_THRESHOLD,
        min_labels: MIN_LABELS,
        verdict: verdict.into(),
        notes,
    })
}

impl Agreement {
    pub fn render(&self) -> String {
        let p = |x: Option<f64>| x.map_or("n/a".into(), |v| format!("{:.0}%", v * 100.0));
        let mut o = format!(
            "Lightning gate (lightning-spec 15.2)\n\nlabeled segments: {}\ntask type agreement: {} of {} (threshold {:.0}%)\noutcome proxy agreement: {} of {} (threshold {:.0}%; proxy decided {})\nverdict: {}\n",
            self.labeled,
            p(self.task_agreement),
            self.task_labeled,
            self.task_threshold * 100.0,
            p(self.outcome_agreement),
            self.outcome_labeled,
            self.outcome_threshold * 100.0,
            p(self.outcome_coverage),
            self.verdict
        );
        for n in &self.notes {
            o.push_str(&format!("- {n}\n"));
        }
        o
    }
}

pub fn find<'a>(segs: &'a [Segment], id: &str) -> Result<&'a Segment> {
    segs.iter().find(|s| s.segment_id == id || s.segment_id.starts_with(id)).with_context(|| format!("no segment {id}"))
}
