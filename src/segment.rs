//! Segments, task types and the outcome proxy (lightning-spec 9.3, 9.4).
//!
//! A segment is one task inside a session: it starts at a `new_task` prompt or after a long idle
//! gap. Everything here is recomputed from stored rows, so changing a heuristic needs no re-ingest.
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::classify::{TASK_TYPES, is_verification, task_index};
use crate::pricing::{PriceTable, Tokens};
use crate::schema::SYNTHETIC_MODEL;
use crate::store::Store;

/// Idle gap that starts a new segment.
pub const GAP_MINUTES: i64 = 30;
/// Share of output tokens one model needs for the segment to have a single pair.
pub const DOMINANT_SHARE: f64 = 0.8;
/// Minimum winning share for a task label; below it the task is `unknown`.
pub const TASK_CONFIDENCE: f64 = 0.5;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Segment {
    pub segment_id: String,
    pub session_id: String,
    pub source: String,
    pub repo: String,
    /// Salted hash of the working directory (or the project slug when unknown).
    pub repo_key: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    /// No model reached the dominant share; excluded from pair comparisons.
    pub mixed: bool,
    pub task_type: String,
    pub task_confidence: f64,
    pub outcome: String,
    pub outcome_signals: Vec<String>,
    pub turns: u64,
    pub subagent_turns: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Estimated cost; None when any turn could not be priced.
    pub cost: Option<f64>,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub denials: u64,
    pub reads: u64,
    pub rereads: u64,
    pub retries: u64,
    pub edits: u64,
    pub corrections: u64,
    pub first_turn_context: Option<i64>,
    pub wall_secs: Option<f64>,
    /// Where the opening prompt lives, for labeling (file id and reference).
    pub prompt_file_id: Option<i64>,
    pub prompt_ref: Option<String>,
}

impl Segment {
    pub fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn pair(&self) -> String {
        format!("{}/{}", self.source, self.model)
    }

    pub fn pair_with_effort(&self) -> String {
        match &self.effort {
            Some(e) => format!("{}/{}@{}", self.source, self.model, e),
            None => self.pair(),
        }
    }

    pub fn size_bucket(&self) -> &'static str {
        match self.turns {
            0..=5 => "small",
            6..=20 => "medium",
            _ => "large",
        }
    }

    /// Comparison stratum: repo and size (lightning-spec 9.5).
    pub fn stratum(&self) -> String {
        format!("{}|{}", self.repo_key, self.size_bucket())
    }

    pub fn success(&self) -> Option<bool> {
        match self.outcome.as_str() {
            "likely_success" => Some(true),
            "likely_failure" => Some(false),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct TurnRow {
    turn_id: String,
    session_id: String,
    is_sub: bool,
    parent: Option<String>,
    project_name: String,
    project: String,
    source: String,
    model: String,
    effort: Option<String>,
    ts: Option<String>,
    tokens: Tokens,
    aborted: bool,
}

#[derive(Debug, Clone)]
struct CallRow {
    session_id: String,
    agent_id: String,
    tool_name: String,
    tool_kind: String,
    args_hash: String,
    target: Option<String>,
    target_kind: Option<String>,
    status: String,
    permission: String,
    cmd_class: Option<String>,
}

#[derive(Debug, Clone)]
struct PromptRow {
    session_id: String,
    ts: Option<String>,
    class: String,
    hint: Option<String>,
    file_id: i64,
    src_ref: Option<String>,
}

#[derive(Default)]
struct Acc {
    prompts: Vec<PromptRow>,
    turns: Vec<TurnRow>,
    start: Option<String>,
    last: Option<String>,
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn gap_exceeded(prev: Option<&String>, next: Option<&String>) -> bool {
    match (prev.and_then(|p| parse_ts(p)), next.and_then(|n| parse_ts(n))) {
        (Some(p), Some(n)) => (n - p).num_minutes() >= GAP_MINUTES,
        _ => false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct SegmentOptions {
    pub since: Option<DateTime<Utc>>,
}

/// Build every segment from the store.
pub fn build(store: &Store, pricing: &PriceTable, opts: &SegmentOptions) -> Result<Vec<Segment>> {
    let c = store.conn();
    let mut repo_keys: HashMap<String, String> = HashMap::new();
    {
        let mut st = c.prepare("SELECT session_id, cwd_hash FROM sessions WHERE cwd_hash IS NOT NULL AND agent_id = ''")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (s, h) = r?;
            repo_keys.insert(s, h);
        }
    }

    let mut turns: Vec<TurnRow> = Vec::new();
    {
        let mut st = c.prepare(
            "SELECT turn_id, session_id, agent_id, is_subagent, parent_session_id, project_name, project,
                    COALESCE(source, 'claude_code'), model, effort, ts, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_5m_tokens, cache_write_1h_tokens, is_aborted
             FROM turns WHERE model != ?1 ORDER BY session_id, ts, turn_id",
        )?;
        let mut rows = st.query([SYNTHETIC_MODEL])?;
        while let Some(r) = rows.next()? {
            turns.push(TurnRow {
                turn_id: r.get(0)?,
                session_id: r.get(1)?,
                is_sub: r.get(3)?,
                parent: r.get(4)?,
                project_name: r.get(5)?,
                project: r.get(6)?,
                source: r.get(7)?,
                model: r.get(8)?,
                effort: r.get(9)?,
                ts: r.get(10)?,
                tokens: Tokens { input: r.get(11)?, output: r.get(12)?, cache_read: r.get(13)?, cache_write_5m: r.get(14)?, cache_write_1h: r.get(15)? },
                aborted: r.get(16)?,
            });
        }
    }

    let mut calls_by_turn: HashMap<String, Vec<CallRow>> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT turn_id, session_id, agent_id, tool_name, COALESCE(tool_kind, 'other'), args_hash, target_hash, target_kind,
                    result_status, permission, cmd_class
             FROM tool_calls ORDER BY session_id, agent_id, file_id, seq",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let turn_id: String = r.get(0)?;
            calls_by_turn.entry(turn_id).or_default().push(CallRow {
                session_id: r.get(1)?,
                agent_id: r.get(2)?,
                tool_name: r.get(3)?,
                tool_kind: r.get(4)?,
                args_hash: r.get(5)?,
                target: r.get(6)?,
                target_kind: r.get(7)?,
                status: r.get(8)?,
                permission: r.get(9)?,
                cmd_class: r.get(10)?,
            });
        }
    }

    let mut prompts: HashMap<String, Vec<PromptRow>> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT session_id, ts, class, task_hint, file_id, src_ref FROM signals
             WHERE kind = 'prompt' AND agent_id = '' AND class IS NOT NULL ORDER BY session_id, ts, id",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let p = PromptRow { session_id: r.get(0)?, ts: r.get(1)?, class: r.get(2)?, hint: r.get(3)?, file_id: r.get(4)?, src_ref: r.get(5)? };
            prompts.entry(p.session_id.clone()).or_default().push(p);
        }
    }

    // Main-agent turns per session; subagent turns per parent session.
    let mut main: BTreeMap<String, Vec<TurnRow>> = BTreeMap::new();
    let mut subs: HashMap<String, Vec<TurnRow>> = HashMap::new();
    for t in turns {
        if t.is_sub {
            let parent = t.parent.clone().unwrap_or_else(|| t.session_id.clone());
            subs.entry(parent).or_default().push(t);
        } else {
            main.entry(t.session_id.clone()).or_default().push(t);
        }
    }

    let cutoff = opts.since.map(|s| s.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let mut out = Vec::new();
    for (session_id, mturns) in main {
        let sprompts = prompts.remove(&session_id).unwrap_or_default();
        // Merge prompts and turns in time order (a prompt sorts before a turn at the same instant).
        let mut events: Vec<(Option<String>, u8, usize)> = Vec::new();
        events.extend(sprompts.iter().enumerate().map(|(i, p)| (p.ts.clone(), 0, i)));
        events.extend(mturns.iter().enumerate().map(|(i, t)| (t.ts.clone(), 1, i)));
        events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let mut accs: Vec<Acc> = Vec::new();
        let mut cur = Acc::default();
        for (ts, kind, i) in events {
            let split = !cur.turns.is_empty()
                && ((kind == 0 && sprompts[i].class == "new_task") || gap_exceeded(cur.last.as_ref(), ts.as_ref()));
            if split {
                accs.push(std::mem::take(&mut cur));
            }
            if cur.start.is_none() {
                cur.start = ts.clone();
            }
            if ts.is_some() {
                cur.last = ts.clone();
            }
            if kind == 0 {
                cur.prompts.push(sprompts[i].clone());
            } else {
                cur.turns.push(mturns[i].clone());
            }
        }
        accs.push(cur);
        // Trailing prompts with no turns (e.g. a correction after the last answer) belong to
        // the segment before them.
        let mut merged: Vec<Acc> = Vec::new();
        for a in accs {
            if a.turns.is_empty() {
                if let Some(prev) = merged.last_mut() {
                    prev.prompts.extend(a.prompts);
                    continue;
                }
                continue;
            }
            merged.push(a);
        }

        let mut sub_turns = subs.remove(&session_id).unwrap_or_default();
        sub_turns.sort_by(|a, b| a.ts.cmp(&b.ts));
        let starts: Vec<Option<String>> = merged.iter().map(|a| a.start.clone()).collect();
        for (idx, acc) in merged.iter().enumerate() {
            let next_start = starts.get(idx + 1).cloned().flatten();
            let attached: Vec<&TurnRow> = sub_turns
                .iter()
                .filter(|t| {
                    let after = match (&t.ts, &acc.start) {
                        (Some(t), Some(s)) => t >= s,
                        _ => idx == 0,
                    };
                    let before = match (&t.ts, &next_start) {
                        (Some(t), Some(n)) => t < n,
                        (_, None) => true,
                        (None, Some(_)) => false,
                    };
                    after && before
                })
                .collect();
            let next_prompt = merged.get(idx + 1).and_then(|n| n.prompts.first()).map(|p| p.class.clone());
            let seg = finish(&session_id, acc, &attached, &calls_by_turn, next_prompt, &repo_keys, pricing, store);
            if let (Some(c), Some(s)) = (&cutoff, &seg.start)
                && s < c
            {
                continue;
            }
            out.push(seg);
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then(a.segment_id.cmp(&b.segment_id)));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    session_id: &str,
    acc: &Acc,
    subs: &[&TurnRow],
    calls_by_turn: &HashMap<String, Vec<CallRow>>,
    next_prompt: Option<String>,
    repo_keys: &HashMap<String, String>,
    pricing: &PriceTable,
    store: &Store,
) -> Segment {
    let first = &acc.turns[0];
    let all: Vec<&TurnRow> = acc.turns.iter().chain(subs.iter().copied()).collect();

    // Dominant model by main-agent output tokens.
    let mut by_model: BTreeMap<(&str, &str), i64> = BTreeMap::new();
    let mut efforts: BTreeMap<&str, u64> = BTreeMap::new();
    for t in &acc.turns {
        *by_model.entry((t.source.as_str(), t.model.as_str())).or_default() += t.tokens.output.unwrap_or(0).max(1);
        if let Some(e) = &t.effort {
            *efforts.entry(e.as_str()).or_default() += 1;
        }
    }
    let total_out: i64 = by_model.values().sum();
    let ((source, model), top) = by_model.iter().max_by_key(|(k, v)| (**v, std::cmp::Reverse(**k))).map(|(k, v)| (*k, *v)).unwrap();
    let mixed = (top as f64) < DOMINANT_SHARE * total_out as f64;
    let effort = efforts.iter().max_by_key(|(k, v)| (**v, std::cmp::Reverse(**k))).map(|(k, _)| k.to_string());

    let (mut input, mut output, mut cr, mut cw) = (0i64, 0i64, 0i64, 0i64);
    let mut cost = Some(0.0);
    for t in &all {
        input += t.tokens.input.unwrap_or(0);
        output += t.tokens.output.unwrap_or(0);
        cr += t.tokens.cache_read.unwrap_or(0);
        cw += t.tokens.cache_write_5m.unwrap_or(0) + t.tokens.cache_write_1h.unwrap_or(0);
        cost = match (cost, pricing.cost(&t.model, t.tokens)) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }

    // Tool-call metrics in order, per agent.
    let mut calls: Vec<&CallRow> = Vec::new();
    for t in &all {
        if let Some(v) = calls_by_turn.get(&t.turn_id) {
            calls.extend(v.iter());
        }
    }
    let (mut errors, mut denials, mut reads, mut rereads, mut retries, mut edits) = (0, 0, 0, 0, 0, 0);
    let mut seen_read: HashMap<(&str, &str), HashSet<&str>> = HashMap::new();
    let mut seen_call: HashMap<(&str, &str, &str), u64> = HashMap::new();
    let mut verifications: Vec<(String, bool)> = Vec::new();
    let (mut commits, mut reverts) = (0u64, 0u64);
    let mut edit_kinds: BTreeMap<String, u64> = BTreeMap::new();
    let (mut searches, mut shells) = (0u64, 0u64);
    for c in &calls {
        if c.status == "error" {
            errors += 1;
        }
        if c.permission == "denied" {
            denials += 1;
        }
        // A retry repeats an identical call; repeated reads are counted as re-reads instead.
        if c.tool_kind != "read" {
            let n = seen_call.entry((c.agent_id.as_str(), c.tool_name.as_str(), c.args_hash.as_str())).or_default();
            if *n > 0 {
                retries += 1;
            }
            *n += 1;
        }
        let key = (c.session_id.as_str(), c.agent_id.as_str());
        match c.tool_kind.as_str() {
            "read" => {
                if let Some(t) = &c.target {
                    reads += 1;
                    if !seen_read.entry(key).or_default().insert(t.as_str()) {
                        rereads += 1;
                    }
                }
            }
            "edit" => {
                edits += 1;
                if let Some(t) = &c.target {
                    seen_read.entry(key).or_default().remove(t.as_str());
                }
                *edit_kinds.entry(c.target_kind.clone().unwrap_or_else(|| "other".into())).or_default() += 1;
            }
            "search" => searches += 1,
            "shell" => {
                shells += 1;
                match c.cmd_class.as_deref() {
                    Some(k) if is_verification(k) && c.status != "unknown" => verifications.push((k.to_string(), c.status == "ok")),
                    Some("git_commit") if c.status != "error" => commits += 1,
                    Some("git_revert") => reverts += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let corrections = acc.prompts.iter().filter(|p| p.class == "correction").count() as u64;
    let aborts = acc.prompts.iter().filter(|p| p.class == "abort").count() as u64 + acc.turns.iter().filter(|t| t.aborted).count() as u64;

    // Task type: prompt keyword hints (opening prompt counts double) plus tool-mix evidence.
    let mut scores = [0f64; 7];
    for (i, p) in acc.prompts.iter().enumerate() {
        if let Some(ix) = p.hint.as_deref().and_then(task_index) {
            scores[ix] += if i == 0 { 2.0 } else { 1.0 };
        }
    }
    let only = |k: &str| edits > 0 && edit_kinds.len() == 1 && edit_kinds.contains_key(k);
    if only("test") {
        scores[3] += 2.0;
    }
    if only("doc") {
        scores[6] += 2.0;
    }
    if only("config") {
        scores[5] += 1.0;
    }
    if edits == 0 && reads + searches > 0 {
        scores[4] += 1.5;
    }
    if verifications.first().is_some_and(|(_, ok)| !ok) && edits > 0 {
        scores[0] += 1.0;
    }
    let sum: f64 = scores.iter().sum();
    let mut order: Vec<usize> = (0..7).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let (best, second) = (scores[order[0]], scores[order[1]]);
    let confidence = if sum > 0.0 { best / sum } else { 0.0 };
    let task_type = if best > 0.0 && best > second && confidence >= TASK_CONFIDENCE { TASK_TYPES[order[0]].to_string() } else { "unknown".to_string() };

    // Outcome proxy (lightning-spec 9.4).
    let mut sig: Vec<String> = Vec::new();
    let last_verify = verifications.last().map(|(k, ok)| {
        sig.push(format!("last_{k}_{}", if *ok { "ok" } else { "failed" }));
        *ok
    });
    if commits > 0 {
        sig.push("commit".into());
    }
    if reverts > 0 {
        sig.push("revert".into());
    }
    if aborts > 0 {
        sig.push("abort".into());
    }
    if corrections > 0 {
        sig.push(format!("corrections={corrections}"));
    }
    // The user's reaction once the agent stopped: a prompt after the last turn of this segment
    // (approval or pushback), else the prompt that opens the next segment.
    let last_turn_ts = acc.turns.last().and_then(|t| t.ts.clone());
    let closing = acc.prompts.last().filter(|p| p.ts.is_some() && p.ts >= last_turn_ts).map(|p| p.class.clone());
    let next_prompt = closing.clone().or(next_prompt);
    if let Some(n) = &next_prompt {
        sig.push(format!("next_prompt={n}"));
    }
    let ended_aborted = acc.turns.last().is_some_and(|t| t.aborted) || closing.as_deref() == Some("abort");
    let strong_negative = last_verify == Some(false) || reverts > 0 || ended_aborted || closing.as_deref() == Some("correction");
    let outcome = if strong_negative || corrections >= 2 {
        "likely_failure"
    } else if corrections == 0 && (last_verify == Some(true) || commits > 0) {
        "likely_success"
    } else if corrections == 0 && edits > 0 && matches!(next_prompt.as_deref(), Some("new_task" | "approval")) {
        "likely_success"
    } else if corrections == 0 && edits == 0 && shells == 0 && matches!(next_prompt.as_deref(), Some("new_task" | "approval")) {
        // Exploration that the user moved on from without pushing back.
        "likely_success"
    } else {
        "unknown"
    };

    let first_ctx = first.tokens.input.map(|i| i + first.tokens.cache_read.unwrap_or(0) + first.tokens.cache_write_5m.unwrap_or(0) + first.tokens.cache_write_1h.unwrap_or(0));
    let wall = match (acc.start.as_deref().and_then(parse_ts), last_turn_ts.as_deref().and_then(parse_ts)) {
        (Some(a), Some(b)) => Some((b - a).num_milliseconds() as f64 / 1000.0),
        _ => None,
    };
    let opening = acc.prompts.first();
    Segment {
        segment_id: store.hasher().hash(&format!("seg|{session_id}|{}|{}", acc.start.clone().unwrap_or_default(), first.turn_id)),
        session_id: session_id.to_string(),
        source: source.to_string(),
        repo: first.project_name.clone(),
        repo_key: repo_keys.get(session_id).cloned().unwrap_or_else(|| first.project.clone()),
        start: acc.start.clone(),
        end: acc.last.clone(),
        model: model.to_string(),
        effort,
        mixed,
        task_type,
        task_confidence: (confidence * 100.0).round() / 100.0,
        outcome: outcome.to_string(),
        outcome_signals: sig,
        turns: all.len() as u64,
        subagent_turns: subs.len() as u64,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cr,
        cache_write_tokens: cw,
        cost,
        tool_calls: calls.len() as u64,
        tool_errors: errors,
        denials,
        reads,
        rereads,
        retries,
        edits,
        corrections,
        first_turn_context: first_ctx,
        wall_secs: wall,
        prompt_file_id: opening.map(|p| p.file_id),
        prompt_ref: opening.and_then(|p| p.src_ref.clone()),
    }
}
