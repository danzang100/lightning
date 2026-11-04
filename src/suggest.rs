//! Suggestions as reviewable diffs (lightning-spec 10). Nothing here writes config; `adopt` does,
//! after a human confirms. Suggestions carry evidence (metrics, n, intervals, segment ids) and
//! never any text copied from logs: instructions come from fixed templates.
use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::analysis::{self, PairKey, Verdict};
use crate::configedit::Edit;
use crate::pricing::{PriceTable, Tokens};
use crate::segment::Segment;
use crate::stats;
use crate::store::Store;

pub const EXPIRY_DAYS: i64 = 30;
/// A failure rate this many points above other pairs (lower bound) makes a rule suggestion.
pub const RULE_MARGIN: f64 = 0.10;
pub const RULE_RATIO: f64 = 1.5;
/// A repo earns a hygiene hint when its median opening context is above this many tokens and
/// at least `HYGIENE_RATIO` times the median of the user's other sessions in the same harness
/// (the harness's own system prompt and tools set a baseline that is not the repo's fault).
pub const HYGIENE_TOKENS: f64 = 60_000.0;
pub const HYGIENE_RATIO: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Default,
    Subagent,
    Rule,
    Hygiene,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "default" => Self::Default,
            "subagent" => Self::Subagent,
            "rule" => Self::Rule,
            "hygiene" => Self::Hygiene,
            o => bail!("unknown suggestion type {o:?} (default, subagent, rule, hygiene)"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Subagent => "subagent",
            Self::Rule => "rule",
            Self::Hygiene => "hygiene",
        }
    }
}

/// Where a change goes. Repo paths are relative to a repo root the user supplies at apply time;
/// user paths are relative to the home directory. Agent files are looked up in both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Target {
    Repo { rel: String },
    User { rel: String },
    Agent { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub target: Target,
    pub edit: Edit,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub repo: Option<String>,
    /// Salted hash of the repo directory; apply checks `--repo` against it.
    pub repo_key: Option<String>,
    pub task: Option<String>,
    pub harness: Option<String>,
    pub pair_from: Option<String>,
    pub pair_to: Option<String>,
    pub agent_type: Option<String>,
    pub metric: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub kind: Kind,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: String,
    pub confidence: String,
    pub rationale: String,
    pub evidence: serde_json::Value,
    pub would_change: String,
    pub expected_effect: String,
    pub scope: Scope,
    pub change: Option<Change>,
}

impl Suggestion {
    fn fingerprint(&self) -> String {
        let s = serde_json::json!({"kind": self.kind, "scope": self.scope, "change": self.change});
        s.to_string()
    }

    pub fn render(&self) -> String {
        let mut o = format!("id: {}   type: {}   confidence: {}   status: {}\n", self.id, self.kind.as_str(), self.confidence, self.status);
        o.push_str(&format!("Rationale: {}\n", self.rationale));
        o.push_str(&format!("Would change if: {}\n", self.would_change));
        o.push_str(&format!("Expected effect: {}\n", self.expected_effect));
        match &self.change {
            Some(c) => {
                let t = match &c.target {
                    Target::Repo { rel } => format!("<repo {}>/{rel}", self.scope.repo.as_deref().unwrap_or("?")),
                    Target::User { rel } => format!("~/{rel}"),
                    Target::Agent { name } => format!(".claude/agents/{name}.md (repo or ~)"),
                };
                o.push_str(&format!("Target: {t}\n"));
                o.push_str(&format!("Apply: lightning apply {}{}\nRollback: lightning rollback {}\n", self.id, if matches!(c.target, Target::Repo { .. }) { " --repo <path>" } else { "" }, self.id));
            }
            None => o.push_str("Advice only (no config change).\n"),
        }
        o
    }
}

fn now_s(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn sample_ids(segs: &[&Segment]) -> Vec<String> {
    segs.iter().take(25).map(|s| s.segment_id.clone()).collect()
}

fn finding_evidence(f: &analysis::Finding, a: &[&Segment], b: &[&Segment]) -> serde_json::Value {
    serde_json::json!({
        "pair_a": f.pair_a, "pair_b": f.pair_b, "n_a": f.n_a, "n_b": f.n_b,
        "success_a": f.success_a, "success_b": f.success_b,
        "success_diff_90": f.diff.as_ref().and_then(|d| d.success_diff.clone()),
        "cost_ratio_90": f.diff.as_ref().and_then(|d| d.cost_ratio.clone()),
        "cost_basis": f.cost_basis, "matched": f.diff.as_ref().map(|d| d.matched),
        "alternation_days": f.alternation_days,
        "segments_a": sample_ids(a), "segments_b": sample_ids(b),
    })
}

fn model_of(pair: &str) -> &str {
    pair.split_once('/').map_or(pair, |x| x.1)
}

fn default_change(harness: &str, model: &str) -> Option<Change> {
    match harness {
        "claude_code" => Some(Change { target: Target::Repo { rel: ".claude/settings.json".into() }, edit: Edit::JsonKey { key: "model".into(), value: model.into() } }),
        "codex" => Some(Change { target: Target::Repo { rel: ".codex/config.toml".into() }, edit: Edit::TomlKey { key: "model".into(), value: model.into() } }),
        // OpenCode wants provider/model and the provider is not in the logs.
        _ => None,
    }
}

const RULES: [(&str, &str, &str); 4] = [
    ("reread", "re-reads files it already read", "Before reading a file again, check whether you already read it in this session and it has not changed since; reuse what you read."),
    ("retry", "repeats identical tool calls", "If a command or tool call fails, change something before retrying it. Never repeat the same call with the same arguments more than once; stop and explain instead."),
    ("tool_error", "has failing tool calls", "Read a file before editing it, and check paths and arguments before calling a tool. When a tool call fails, read the error before the next attempt."),
    ("denial", "runs into permission denials", "Ask before running commands that change files outside the repo, install packages or use the network; prefer read-only commands while exploring."),
];

fn metric_hit(metric: &str, s: &Segment) -> Option<bool> {
    match metric {
        "reread" => (s.reads > 0).then_some(s.rereads > 0),
        "retry" => (s.tool_calls > 0).then_some(s.retries > 0),
        "tool_error" => (s.tool_calls > 0).then_some(s.tool_errors > 0),
        "denial" => (s.tool_calls > 0).then_some(s.denials > 0),
        _ => None,
    }
}

fn rule_change(harness: &str, pair: &str, metric: &str, text: &str) -> Option<Change> {
    let rel = match harness {
        "claude_code" => ".claude/CLAUDE.md",
        "codex" => ".codex/AGENTS.md",
        "opencode" => ".config/opencode/AGENTS.md",
        _ => return None,
    };
    let model = model_of(pair);
    let marker = format!("rule:{model}:{metric}");
    let body = format!("## When running as {model}\n\n{text}");
    Some(Change { target: Target::User { rel: rel.into() }, edit: Edit::MarkdownSection { marker, body } })
}

#[derive(Debug, Clone)]
pub struct SubagentRun {
    pub agent_type: String,
    pub model: String,
    pub cost: Option<f64>,
    pub tokens: i64,
    pub tool_calls: u64,
    pub errored: bool,
    pub start: Option<String>,
}

/// One row per subagent run (Claude Code subagent files carry `agentType`).
pub fn subagent_runs(store: &Store, pricing: &PriceTable) -> Result<Vec<SubagentRun>> {
    let c = store.conn();
    let mut errs: HashMap<String, (u64, u64)> = HashMap::new();
    let mut st = c.prepare("SELECT turn_id, COUNT(*), SUM(result_status = 'error') FROM tool_calls GROUP BY turn_id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
        let (t, n, e) = r?;
        errs.insert(t, (n as u64, e as u64));
    }
    let mut runs: BTreeMap<(String, String), SubagentRun> = BTreeMap::new();
    let mut st = c.prepare(
        "SELECT session_id, agent_id, agent_type, model, ts, turn_id, input_tokens, output_tokens, cache_read_tokens,
                cache_write_5m_tokens, cache_write_1h_tokens
         FROM turns WHERE is_subagent = 1 AND agent_type IS NOT NULL AND model != '<synthetic>' ORDER BY ts",
    )?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let key = (r.get::<_, String>(0)?, r.get::<_, String>(1)?);
        let tokens = Tokens { input: r.get(6)?, output: r.get(7)?, cache_read: r.get(8)?, cache_write_5m: r.get(9)?, cache_write_1h: r.get(10)? };
        let model: String = r.get(3)?;
        let (n, e) = errs.get(&r.get::<_, String>(5)?).copied().unwrap_or((0, 0));
        let run = runs.entry(key).or_insert_with(|| SubagentRun {
            agent_type: String::new(),
            model: model.clone(),
            cost: Some(0.0),
            tokens: 0,
            tool_calls: 0,
            errored: false,
            start: None,
        });
        run.agent_type = r.get(2)?;
        if run.start.is_none() {
            run.start = r.get(4)?;
        }
        run.cost = match (run.cost, pricing.cost(&model, tokens)) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        run.tokens += tokens.input.unwrap_or(0) + tokens.output.unwrap_or(0) + tokens.cache_read.unwrap_or(0) + tokens.cache_write_5m.unwrap_or(0) + tokens.cache_write_1h.unwrap_or(0);
        run.tool_calls += n;
        run.errored |= e > 0;
    }
    Ok(runs.into_values().collect())
}

/// Build candidate suggestions from the data (not yet stored).
pub fn generate(segs: &[Segment], runs: &[SubagentRun], kinds: &[Kind], min_n: u64, now: DateTime<Utc>) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let ts = now_s(now);
    let expires = now_s(now + Duration::days(EXPIRY_DAYS));
    let mk = |kind: Kind, confidence: &str, rationale: String, evidence: serde_json::Value, would: String, effect: String, scope: Scope, change: Option<Change>| Suggestion {
        id: String::new(),
        kind,
        status: "pending".into(),
        created_at: ts.clone(),
        updated_at: ts.clone(),
        expires_at: expires.clone(),
        confidence: confidence.into(),
        rationale,
        evidence,
        would_change: would,
        expected_effect: effect,
        scope,
        change,
    };
    let comparable = analysis::comparable(segs);

    if kinds.contains(&Kind::Default) {
        // Project default: within one repo and harness, the most used pair vs cheaper ones.
        let mut by: BTreeMap<(String, String), BTreeMap<String, Vec<&Segment>>> = BTreeMap::new();
        for s in &comparable {
            by.entry((s.repo_key.clone(), s.source.clone())).or_default().entry(s.pair()).or_default().push(s);
        }
        for ((repo_key, harness), pairs) in by {
            let decided = |v: &Vec<&Segment>| v.iter().filter(|s| s.success().is_some()).count() as u64;
            let Some((pa, va)) = pairs.iter().max_by_key(|(p, v)| (v.len(), std::cmp::Reverse((*p).clone()))) else { continue };
            if decided(va) < min_n {
                continue;
            }
            for (pb, vb) in pairs.iter().filter(|(p, _)| *p != pa) {
                let f = analysis::compare_pairs_by("all", pa, va, pb, vb, min_n, |s| format!("{}|{}", s.task_type, s.size_bucket()));
                if f.verdict != Verdict::Recommend {
                    continue;
                }
                let repo = va[0].repo.clone();
                let cr = f.diff.as_ref().and_then(|d| d.cost_ratio.clone()).unwrap();
                out.push(mk(
                    Kind::Default,
                    &f.confidence,
                    format!("In {repo}, {pb} matches {pa} on proxy success across task types at ~{:.0}% of the {}.", cr.value * 100.0, if f.cost_basis == "usd" { "cost" } else { "tokens" }),
                    finding_evidence(&f, va, vb),
                    f.would_change.clone(),
                    format!("{} per segment falls to {:.0}%-{:.0}% of today's; proxy success unchanged", if f.cost_basis == "usd" { "cost" } else { "tokens" }, cr.lo * 100.0, cr.hi * 100.0),
                    Scope { repo: Some(repo), repo_key: Some(repo_key.clone()), harness: Some(harness.clone()), pair_from: Some(pa.clone()), pair_to: Some(pb.clone()), ..Scope::default() },
                    default_change(&harness, model_of(pb)),
                ));
            }
        }
        // Task-level routing advice (no config file can express it; `pick` and MCP use it).
        for f in analysis::findings(segs, PairKey::Pair, min_n).into_iter().filter(|f| f.verdict == Verdict::Recommend) {
            let a: Vec<&Segment> = comparable.iter().filter(|s| s.task_type == f.task && s.pair() == f.pair_a).copied().collect();
            let b: Vec<&Segment> = comparable.iter().filter(|s| s.task_type == f.task && s.pair() == f.pair_b).copied().collect();
            out.push(mk(
                Kind::Default,
                &f.confidence,
                format!("For {} work, use {} instead of {}: {}", f.task, f.pair_b, f.pair_a, f.summary),
                finding_evidence(&f, &a, &b),
                f.would_change.clone(),
                "lower cost per segment for this task type; proxy success unchanged".into(),
                Scope { task: Some(f.task.clone()), pair_from: Some(f.pair_a.clone()), pair_to: Some(f.pair_b.clone()), ..Scope::default() },
                None,
            ));
        }
    }

    if kinds.contains(&Kind::Subagent) {
        let mut by: BTreeMap<String, BTreeMap<String, Vec<&SubagentRun>>> = BTreeMap::new();
        for r in runs {
            by.entry(r.agent_type.clone()).or_default().entry(r.model.clone()).or_default().push(r);
        }
        for (agent, models) in by {
            let eligible: Vec<(&String, &Vec<&SubagentRun>)> = models.iter().filter(|(_, v)| v.len() as u64 >= min_n).collect();
            if eligible.len() < 2 {
                continue;
            }
            let all_priced = eligible.iter().all(|(_, v)| v.iter().all(|r| r.cost.is_some()));
            let cost = |r: &SubagentRun| if all_priced { r.cost.unwrap_or(0.0) } else { r.tokens as f64 };
            let mean = |v: &Vec<&SubagentRun>| stats::mean(&v.iter().map(|r| cost(r)).collect::<Vec<_>>()).unwrap_or(0.0);
            let (ma, va) = *eligible.iter().max_by(|x, y| mean(x.1).total_cmp(&mean(y.1))).unwrap();
            for (mb, vb) in eligible.iter().filter(|(m, _)| *m != ma) {
                let obs = |v: &Vec<&SubagentRun>| -> Vec<stats::Obs> { v.iter().map(|r| stats::Obs { stratum: "all".into(), success: Some(!r.errored), cost: cost(r) }).collect() };
                let d = stats::compare(&obs(va), &obs(vb), analysis::BOOT_REPS, analysis::SEED);
                let (Some(sd), Some(cr)) = (d.success_diff.clone(), d.cost_ratio.clone()) else { continue };
                if sd.lo > -analysis::SUCCESS_MARGIN && cr.hi < analysis::COST_RATIO_MAX {
                    out.push(mk(
                        Kind::Subagent,
                        if va.len().min(vb.len()) as u64 >= 3 * min_n { "high" } else { "medium" },
                        format!("Subagent {agent}: runs on {mb} finish without tool errors as often as on {ma} at ~{:.0}% of the {}.", cr.value * 100.0, if all_priced { "cost" } else { "tokens" }),
                        serde_json::json!({"agent_type": agent, "model_a": ma, "model_b": mb, "runs_a": va.len(), "runs_b": vb.len(), "error_free_diff_90": sd, "cost_ratio_90": cr}),
                        format!("if {mb} runs start erring more than {} points more often than {ma} runs", (analysis::SUCCESS_MARGIN * 100.0) as i64),
                        format!("subagent cost falls to {:.0}%-{:.0}% of today's", cr.lo * 100.0, cr.hi * 100.0),
                        Scope { agent_type: Some(agent.clone()), harness: Some("claude_code".into()), pair_from: Some((*ma).clone()), pair_to: Some((*mb).clone()), ..Scope::default() },
                        Some(Change { target: Target::Agent { name: agent.clone() }, edit: Edit::Frontmatter { key: "model".into(), value: (*mb).clone() } }),
                    ));
                }
            }
        }
    }

    if kinds.contains(&Kind::Rule) {
        let mut by_pair: BTreeMap<String, Vec<&Segment>> = BTreeMap::new();
        for s in &comparable {
            by_pair.entry(s.pair()).or_default().push(s);
        }
        for (metric, what, text) in RULES {
            for (pair, segs_p) in &by_pair {
                let hits = |v: &[&Segment]| -> (u64, u64) {
                    let h: Vec<bool> = v.iter().filter_map(|s| metric_hit(metric, s)).collect();
                    (h.iter().filter(|x| **x).count() as u64, h.len() as u64)
                };
                let others: Vec<&Segment> = by_pair.iter().filter(|(p, _)| *p != pair).flat_map(|(_, v)| v.iter().copied()).collect();
                let ((k1, n1), (k2, n2)) = (hits(&others), hits(segs_p));
                if n1 < min_n || n2 < min_n {
                    continue;
                }
                let Some(diff) = stats::newcombe(k1, n1, k2, n2) else { continue };
                let (r1, r2) = (k1 as f64 / n1 as f64, k2 as f64 / n2 as f64);
                if diff.lo > RULE_MARGIN && r2 >= RULE_RATIO * r1.max(0.01) {
                    let harness = segs_p[0].source.clone();
                    out.push(mk(
                        Kind::Rule,
                        if n2.min(n1) >= 3 * min_n { "high" } else { "medium" },
                        format!("{pair} {what} in {:.0}% of segments vs {:.0}% for other pairs. A short instruction scoped to this model targets it.", r2 * 100.0, r1 * 100.0),
                        serde_json::json!({"metric": metric, "pair": pair, "rate_pair": r2, "n_pair": n2, "rate_others": r1, "n_others": n1, "difference_95": diff, "segments": sample_ids(segs_p)}),
                        format!("if {pair}'s {metric} rate falls to within {} points of other pairs", (RULE_MARGIN * 100.0) as i64),
                        format!("{metric} rate for {pair} drops toward {:.0}%", r1 * 100.0),
                        Scope { harness: Some(harness.clone()), pair_from: Some(pair.clone()), metric: Some(metric.into()), ..Scope::default() },
                        rule_change(&harness, pair, metric, text),
                    ));
                }
            }
        }
    }

    if kinds.contains(&Kind::Hygiene) {
        let mut by_repo: BTreeMap<(String, String), Vec<&Segment>> = BTreeMap::new();
        let mut by_harness: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for s in segs {
            by_repo.entry((s.source.clone(), s.repo_key.clone())).or_default().push(s);
            if let Some(c) = s.first_turn_context {
                by_harness.entry(s.source.clone()).or_default().push(c as f64);
            }
        }
        for ((harness, key), v) in by_repo {
            let ctx: Vec<f64> = v.iter().filter_map(|s| s.first_turn_context.map(|x| x as f64)).collect();
            if (ctx.len() as u64) < (min_n / 2).max(5) {
                continue;
            }
            let med = stats::median(&ctx).unwrap_or(0.0);
            let baseline = stats::median(&by_harness[&harness]).unwrap_or(0.0);
            if med > HYGIENE_TOKENS && med > HYGIENE_RATIO * baseline {
                let repo = v[0].repo.clone();
                out.push(mk(
                    Kind::Hygiene,
                    "medium",
                    format!(
                        "In {repo}, sessions start with a median of {:.0}k tokens of context before any work, against {:.0}k across your {harness} sessions.",
                        med / 1000.0,
                        baseline / 1000.0
                    ),
                    serde_json::json!({"median_first_turn_context": med, "harness_median": baseline, "n": ctx.len(), "segments": sample_ids(&v)}),
                    format!("if the median drops below {:.0}k tokens or the gap to your other repos closes", (HYGIENE_RATIO * baseline).max(HYGIENE_TOKENS) / 1000.0),
                    "smaller first turns: trim always-loaded instruction files, imported docs and unused MCP tools".into(),
                    Scope { repo: Some(repo), repo_key: Some(key), ..Scope::default() },
                    None,
                ));
            }
        }
    }
    out
}

fn load(row: &rusqlite::Row) -> rusqlite::Result<Suggestion> {
    let body: String = row.get(0)?;
    let mut s: Suggestion = serde_json::from_str(&body).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    s.status = row.get(1)?;
    Ok(s)
}

pub fn get(store: &Store, id: &str) -> Result<Suggestion> {
    store
        .conn()
        .query_row("SELECT body, status FROM suggestions WHERE id = ?1", params![id], load)
        .optional()?
        .with_context(|| format!("no suggestion {id}"))
}

pub fn list(store: &Store, status: Option<&str>) -> Result<Vec<Suggestion>> {
    let mut st = store.conn().prepare("SELECT body, status FROM suggestions WHERE ?1 IS NULL OR status = ?1 ORDER BY id")?;
    let v = st.query_map(params![status], load)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v)
}

pub fn set_status(store: &Store, id: &str, status: &str, now: DateTime<Utc>) -> Result<()> {
    let n = store.conn().execute("UPDATE suggestions SET status = ?2, updated_at = ?3 WHERE id = ?1", params![id, status, now_s(now)])?;
    if n == 0 {
        bail!("no suggestion {id}");
    }
    Ok(())
}

/// Store new suggestions, refresh pending ones with new evidence, expire stale ones.
/// Applied, rolled back and dismissed suggestions are never resurrected.
pub fn save(store: &Store, fresh: Vec<Suggestion>, now: DateTime<Utc>) -> Result<Vec<Suggestion>> {
    let c = store.conn();
    c.execute("UPDATE suggestions SET status = 'expired' WHERE status = 'pending' AND expires_at < ?1", params![now_s(now)])?;
    let mut touched = Vec::new();
    for mut s in fresh {
        let fp = s.fingerprint();
        let existing: Option<(String, String, String)> =
            c.query_row("SELECT id, status, created_at FROM suggestions WHERE fingerprint = ?1", params![fp], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        match existing {
            Some((id, status, created)) => {
                if status != "pending" && status != "expired" {
                    continue;
                }
                s.id = id;
                s.created_at = created;
                c.execute(
                    "UPDATE suggestions SET status = 'pending', updated_at = ?2, expires_at = ?3, body = ?4 WHERE id = ?1",
                    params![s.id, s.updated_at, s.expires_at, serde_json::to_string(&s)?],
                )?;
            }
            None => {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM suggestions", [], |r| r.get(0))?;
                s.id = format!("sug-{:04}", n + 1);
                c.execute(
                    "INSERT INTO suggestions(id, fingerprint, kind, status, created_at, updated_at, expires_at, body) VALUES (?1,?2,?3,'pending',?4,?5,?6,?7)",
                    params![s.id, fp, s.kind.as_str(), s.created_at, s.updated_at, s.expires_at, serde_json::to_string(&s)?],
                )?;
            }
        }
        touched.push(s);
    }
    Ok(touched)
}
