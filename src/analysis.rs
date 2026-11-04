//! Pair x task comparison with confidence (lightning-spec 6.1, 9.2, 9.5).
//!
//! The unit is the (harness, model) pair. Every number carries its n; cells below the minimum n
//! say "insufficient data"; differences are stratified by repo and size and come with intervals.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::segment::Segment;
use crate::stats::{self, Interval, Obs, PairDiff};

pub const DEFAULT_MIN_N: u64 = 20;
/// Largest success drop (B - A) still treated as "no worse".
pub const SUCCESS_MARGIN: f64 = 0.10;
/// B must credibly cost less than this fraction of A.
pub const COST_RATIO_MAX: f64 = 0.9;
pub const BOOT_REPS: usize = 2000;
pub const SEED: u64 = 0x11_6874_6e69_6e67;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairKey {
    Pair,
    PairEffort,
}

impl PairKey {
    pub fn of(self, s: &Segment) -> String {
        match self {
            Self::Pair => s.pair(),
            Self::PairEffort => s.pair_with_effort(),
        }
    }
}

/// Segments that may enter a comparison: one dominant pair and a priced-or-tokens cost.
pub fn comparable(segs: &[Segment]) -> Vec<&Segment> {
    segs.iter().filter(|s| !s.mixed).collect()
}

/// "usd" when every segment has a cost, else "tokens" (local and unpriced models).
pub fn cost_basis(segs: &[&Segment]) -> &'static str {
    if !segs.is_empty() && segs.iter().all(|s| s.cost.is_some()) { "usd" } else { "tokens" }
}

pub fn cost_of(s: &Segment, basis: &str) -> f64 {
    if basis == "usd" { s.cost.unwrap_or(0.0) } else { s.total_tokens() as f64 }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Cell {
    pub pair: String,
    pub task: String,
    pub segments: u64,
    pub decided: u64,
    pub successes: u64,
    pub success_rate: Option<f64>,
    pub success_lo: Option<f64>,
    pub success_hi: Option<f64>,
    pub sufficient: bool,
    pub cost_basis: String,
    pub mean_cost: Option<f64>,
    pub median_cost: Option<f64>,
    pub median_wall_secs: Option<f64>,
    pub tool_error_rate: Option<f64>,
    pub reread_rate: Option<f64>,
    pub retry_rate: Option<f64>,
    pub corrections_per_segment: Option<f64>,
    pub median_first_turn_context: Option<f64>,
}

fn ratio(n: u64, d: u64) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

pub fn cell(pair: &str, task: &str, segs: &[&Segment], min_n: u64) -> Cell {
    let decided: Vec<bool> = segs.iter().filter_map(|s| s.success()).collect();
    let k = decided.iter().filter(|x| **x).count() as u64;
    let n = decided.len() as u64;
    let ci = stats::wilson(k, n);
    let basis = cost_basis(segs);
    let costs: Vec<f64> = segs.iter().map(|s| cost_of(s, basis)).collect();
    let walls: Vec<f64> = segs.iter().filter_map(|s| s.wall_secs).collect();
    let ctx: Vec<f64> = segs.iter().filter_map(|s| s.first_turn_context.map(|x| x as f64)).collect();
    let sum = |f: fn(&Segment) -> u64| segs.iter().map(|s| f(s)).sum::<u64>();
    Cell {
        pair: pair.to_string(),
        task: task.to_string(),
        segments: segs.len() as u64,
        decided: n,
        successes: k,
        success_rate: ratio(k, n),
        success_lo: ci.map(|c| c.0),
        success_hi: ci.map(|c| c.1),
        sufficient: n >= min_n,
        cost_basis: basis.to_string(),
        mean_cost: stats::mean(&costs),
        median_cost: stats::median(&costs),
        median_wall_secs: stats::median(&walls),
        tool_error_rate: ratio(sum(|s| s.tool_errors), sum(|s| s.tool_calls)),
        reread_rate: ratio(sum(|s| s.rereads), sum(|s| s.reads)),
        retry_rate: ratio(sum(|s| s.retries), sum(|s| s.tool_calls)),
        corrections_per_segment: (!segs.is_empty()).then(|| sum(|s| s.corrections) as f64 / segs.len() as f64),
        median_first_turn_context: stats::median(&ctx),
    }
}

/// Cells grouped by pair (task = "all") or by pair and task.
pub fn cells(segs: &[Segment], key: PairKey, by_task: bool, min_n: u64) -> Vec<Cell> {
    let mut groups: BTreeMap<(String, String), Vec<&Segment>> = BTreeMap::new();
    for s in comparable(segs) {
        let task = if by_task { s.task_type.clone() } else { "all".to_string() };
        groups.entry((key.of(s), task)).or_default().push(s);
    }
    groups.iter().map(|((p, t), v)| cell(p, t, v, min_n)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// B is no worse on the proxy and credibly cheaper.
    Recommend,
    /// B's proxy success is credibly lower.
    Worse,
    NoClearDifference,
    InsufficientData,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub task: String,
    /// Baseline (usually the more expensive pair).
    pub pair_a: String,
    pub pair_b: String,
    pub verdict: Verdict,
    pub n_a: u64,
    pub n_b: u64,
    pub success_a: Option<f64>,
    pub success_b: Option<f64>,
    pub diff: Option<PairDiff>,
    pub cost_basis: String,
    pub confidence: String,
    /// Repo-days where both pairs were used on this task type (the strongest evidence).
    pub alternation_days: u64,
    pub summary: String,
    pub would_change: String,
}

fn alternation_days(a: &[&Segment], b: &[&Segment]) -> u64 {
    let day = |s: &&Segment| format!("{}|{}", s.repo_key, s.start.as_deref().unwrap_or("").get(..10).unwrap_or(""));
    let da: BTreeSet<String> = a.iter().map(day).collect();
    let db: BTreeSet<String> = b.iter().map(day).collect();
    da.intersection(&db).count() as u64
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// Compare pair B against baseline A on the given segments (already filtered to one task).
pub fn compare_pairs(task: &str, pair_a: &str, a: &[&Segment], pair_b: &str, b: &[&Segment], min_n: u64) -> Finding {
    let decided = |v: &[&Segment]| v.iter().filter(|s| s.success().is_some()).count() as u64;
    let (n_a, n_b) = (decided(a), decided(b));
    let rate = |v: &[&Segment]| {
        let d: Vec<bool> = v.iter().filter_map(|s| s.success()).collect();
        ratio(d.iter().filter(|x| **x).count() as u64, d.len() as u64)
    };
    let both: Vec<&Segment> = a.iter().chain(b.iter()).copied().collect();
    let basis = cost_basis(&both);
    let obs = |v: &[&Segment]| -> Vec<Obs> {
        v.iter().map(|s| Obs { stratum: s.stratum(), success: s.success(), cost: cost_of(s, basis) }).collect()
    };
    let alternation = alternation_days(a, b);
    let mut f = Finding {
        task: task.to_string(),
        pair_a: pair_a.to_string(),
        pair_b: pair_b.to_string(),
        verdict: Verdict::InsufficientData,
        n_a,
        n_b,
        success_a: rate(a),
        success_b: rate(b),
        diff: None,
        cost_basis: basis.to_string(),
        confidence: "low".to_string(),
        alternation_days: alternation,
        summary: String::new(),
        would_change: String::new(),
    };
    if n_a < min_n || n_b < min_n {
        f.summary = format!("{task}: insufficient data ({pair_a} n={n_a}, {pair_b} n={n_b}; need {min_n} decided segments each)");
        f.would_change = format!("{} more decided segments for the smaller side", min_n.saturating_sub(n_a.min(n_b)));
        return f;
    }
    let d = stats::compare(&obs(a), &obs(b), BOOT_REPS, SEED);
    let (Some(sd), Some(cr)) = (d.success_diff.clone(), d.cost_ratio.clone()) else {
        f.summary = format!("{task}: could not estimate a difference between {pair_a} and {pair_b}");
        f.diff = Some(d);
        return f;
    };
    f.verdict = if sd.lo > -SUCCESS_MARGIN && cr.hi < COST_RATIO_MAX {
        Verdict::Recommend
    } else if sd.hi < -SUCCESS_MARGIN {
        Verdict::Worse
    } else {
        Verdict::NoClearDifference
    };
    let width = sd.hi - sd.lo;
    f.confidence = if n_a.min(n_b) >= 3 * min_n && width < 0.2 && d.matched {
        "high"
    } else if width < 0.35 && d.matched {
        "medium"
    } else {
        "low"
    }
    .to_string();
    let cost_word = if basis == "usd" { "cost" } else { "tokens" };
    f.summary = match f.verdict {
        Verdict::Recommend => format!(
            "{task}: {pair_b} is within {} points of {pair_a} on proxy success at ~{} of the {cost_word} ({} confidence)",
            (sd.value.abs() * 100.0).round(),
            pct(cr.value),
            f.confidence
        ),
        Verdict::Worse => format!(
            "{task}: {pair_b} has lower proxy success than {pair_a} ({} vs {}; difference {} to {} points)",
            f.success_b.map(pct).unwrap_or_default(),
            f.success_a.map(pct).unwrap_or_default(),
            (sd.lo * 100.0).round(),
            (sd.hi * 100.0).round()
        ),
        _ => format!(
            "{task}: {pair_a} and {pair_b} are indistinguishable within intervals (success difference {} to {} points, {cost_word} ratio {:.2} to {:.2})",
            (sd.lo * 100.0).round(),
            (sd.hi * 100.0).round(),
            cr.lo,
            cr.hi
        ),
    };
    f.would_change = match f.verdict {
        Verdict::Recommend => format!(
            "if {pair_b}'s proxy success falls more than {} points below {pair_a}'s as n grows, or its {cost_word} edge shrinks below 10%",
            (SUCCESS_MARGIN * 100.0) as i64
        ),
        Verdict::Worse => format!("if more {pair_b} segments bring its success interval back within {} points", (SUCCESS_MARGIN * 100.0) as i64),
        _ => "more segments, especially days where you alternate both pairs on similar work".to_string(),
    };
    if !d.matched {
        f.would_change.push_str("; the two pairs share no repo/size stratum yet, so this is an unmatched comparison");
    }
    f.diff = Some(d);
    f
}

/// For each task type, compare every sufficiently sampled pair against the most expensive one.
pub fn findings(segs: &[Segment], key: PairKey, min_n: u64) -> Vec<Finding> {
    let mut by_task: BTreeMap<String, BTreeMap<String, Vec<&Segment>>> = BTreeMap::new();
    for s in comparable(segs) {
        if s.task_type == "unknown" {
            continue;
        }
        by_task.entry(s.task_type.clone()).or_default().entry(key.of(s)).or_default().push(s);
    }
    let mut out = Vec::new();
    for (task, pairs) in by_task {
        let all: Vec<&Segment> = pairs.values().flatten().copied().collect();
        let basis = cost_basis(&all);
        let mut ranked: Vec<(&String, &Vec<&Segment>, f64)> = pairs
            .iter()
            .filter(|(_, v)| v.iter().filter(|s| s.success().is_some()).count() as u64 >= min_n)
            .map(|(p, v)| (p, v, stats::mean(&v.iter().map(|s| cost_of(s, basis)).collect::<Vec<_>>()).unwrap_or(0.0)))
            .collect();
        if ranked.len() < 2 {
            continue;
        }
        ranked.sort_by(|x, y| y.2.total_cmp(&x.2).then(x.0.cmp(y.0)));
        let (pa, va, _) = ranked[0];
        for (pb, vb, _) in &ranked[1..] {
            out.push(compare_pairs(&task, pa, va, pb, vb, min_n));
        }
    }
    out
}

/// Resolve a user-typed pair ("harness/model", "harness/model@effort" or a bare model).
pub fn resolve_pair(segs: &[Segment], wanted: &str) -> Result<(PairKey, String)> {
    let key = if wanted.contains('@') { PairKey::PairEffort } else { PairKey::Pair };
    let known: BTreeSet<String> = segs.iter().map(|s| key.of(s)).collect();
    if known.contains(wanted) {
        return Ok((key, wanted.to_string()));
    }
    let hits: Vec<&String> = known.iter().filter(|p| p.split_once('/').map(|x| x.1) == Some(wanted)).collect();
    match hits.as_slice() {
        [one] => Ok((key, (*one).clone())),
        [] => bail!("no segments for pair {wanted:?}; known pairs: {}", known.into_iter().collect::<Vec<_>>().join(", ")),
        many => bail!("{wanted:?} matches several pairs ({}); use harness/model", many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Ranked {
    pub pair: String,
    pub proxy_success: Option<f64>,
    pub success_lo: Option<f64>,
    pub cost_per_segment: Option<f64>,
    pub cost_basis: String,
    pub n: u64,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    pub task_type: String,
    pub task_type_confidence: f64,
    pub ranking: Vec<Ranked>,
    pub verdict: String,
    pub caveats: Vec<String>,
}

/// Heuristic `recommend_pair` (lightning-spec appendix C): classify the description, then rank
/// pairs for that task type (in `repo` when given) by success lower bound, then cost.
pub fn recommend(segs: &[Segment], task_description: &str, repo: Option<&str>, min_n: u64) -> Recommendation {
    let scores = crate::classify::task_scores(task_description);
    let total: u32 = scores.iter().sum();
    let (best, top) = scores.iter().enumerate().max_by_key(|(i, v)| (**v, std::cmp::Reverse(*i))).map(|(i, v)| (i, *v)).unwrap_or((0, 0));
    let task = if top > 0 { crate::classify::TASK_TYPES[best].to_string() } else { "unknown".to_string() };
    let conf = if total > 0 { top as f64 / total as f64 } else { 0.0 };
    let mut caveats = vec!["proxy success is a heuristic estimate, not a measured pass rate".to_string()];
    let pool: Vec<Segment> = segs
        .iter()
        .filter(|s| task == "unknown" || s.task_type == task)
        .filter(|s| repo.is_none_or(|r| s.repo.eq_ignore_ascii_case(r)))
        .cloned()
        .collect();
    if task == "unknown" {
        caveats.push("could not classify the task description; ranking uses all task types".into());
    }
    let mut ranking: Vec<Ranked> = cells(&pool, PairKey::Pair, false, min_n)
        .into_iter()
        .map(|c| Ranked {
            confidence: if c.sufficient { if c.decided >= 3 * min_n { "high" } else { "medium" } } else { "insufficient" }.to_string(),
            pair: c.pair,
            proxy_success: c.success_rate,
            success_lo: c.success_lo,
            cost_per_segment: c.mean_cost,
            cost_basis: c.cost_basis,
            n: c.decided,
        })
        .collect();
    ranking.sort_by(|a, b| {
        (b.confidence != "insufficient")
            .cmp(&(a.confidence != "insufficient"))
            .then(b.success_lo.unwrap_or(0.0).total_cmp(&a.success_lo.unwrap_or(0.0)))
            .then(a.cost_per_segment.unwrap_or(f64::MAX).total_cmp(&b.cost_per_segment.unwrap_or(f64::MAX)))
    });
    let sufficient = ranking.iter().filter(|r| r.confidence != "insufficient").count();
    let verdict = if sufficient >= 1 && conf >= crate::segment::TASK_CONFIDENCE { "recommend" } else { "insufficient_data" };
    if sufficient == 0 {
        caveats.push(format!("no pair has {min_n} decided segments for this task type yet"));
    }
    if ranking.iter().map(|r| r.cost_basis.as_str()).collect::<BTreeSet<_>>().len() > 1 {
        caveats.push("some pairs are unpriced; their cost is in tokens, not dollars".into());
    }
    Recommendation { task_type: task, task_type_confidence: (conf * 100.0).round() / 100.0, ranking, verdict: verdict.to_string(), caveats }
}

pub fn interval_text(i: &Option<Interval>, scale: f64, unit: &str) -> String {
    match i {
        Some(i) => format!("{:.1}{unit} [{:.1}, {:.1}]", i.value * scale, i.lo * scale, i.hi * scale),
        None => "n/a".to_string(),
    }
}
