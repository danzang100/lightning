//! `apply`, `rollback` and the closed-loop `review` (lightning-spec 10.3, 10.4).
//!
//! Apply shows the diff, asks, writes only allowlisted paths and records the file's hash and
//! content before and after, so rollback can restore it exactly. Review compares a window before
//! adoption with the time since, on the targeted metric and on the quality proxy.
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::analysis;
use crate::configedit::{self, unified_diff};
use crate::redact::normalize_path;
use crate::segment::Segment;
use crate::stats;
use crate::store::Store;
use crate::suggest::{self, Kind, SubagentRun, Suggestion, Target};

/// Where user-level and repo-level config live. The CLI fills it from the environment.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub home: PathBuf,
    pub repo: Option<PathBuf>,
}

fn sha(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn ts(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Resolve the absolute file a suggestion targets, enforcing the allowlist and repo identity.
pub fn target_path(store: &Store, s: &Suggestion, ctx: &Ctx) -> Result<(PathBuf, String)> {
    let change = s.change.as_ref().context("this suggestion is advice only; there is nothing to apply")?;
    match &change.target {
        Target::User { rel } => Ok((configedit::resolve(&ctx.home, rel, true)?, format!("~/{rel}"))),
        Target::Repo { rel } => {
            let repo = ctx.repo.as_ref().context("this change targets a repo; pass --repo <path to the repo>")?;
            if let Some(key) = &s.scope.repo_key {
                let canon = repo.canonicalize().unwrap_or_else(|_| repo.clone());
                let candidates = [repo.to_string_lossy().into_owned(), canon.to_string_lossy().trim_start_matches(r"\\?\").to_string()];
                if !candidates.iter().any(|p| store.hasher().hash(&normalize_path(p, None)) == *key) {
                    bail!("{} is not the repo this suggestion is about ({}); pass --repo with that repo's path", repo.display(), s.scope.repo.as_deref().unwrap_or("?"));
                }
            }
            Ok((configedit::resolve(repo, rel, false)?, rel.clone()))
        }
        Target::Agent { name } => {
            let rel = format!(".claude/agents/{name}.md");
            if let Some(repo) = &ctx.repo {
                let p = configedit::resolve(repo, &rel, false)?;
                if p.is_file() {
                    return Ok((p, rel));
                }
            }
            let p = configedit::resolve(&ctx.home, &rel, true)?;
            if p.is_file() {
                return Ok((p, format!("~/{rel}")));
            }
            bail!("no agent definition {rel} in the repo or home directory (plugin-provided agents cannot be edited here)")
        }
    }
}

/// The diff `apply` would write, without writing anything.
pub fn preview(store: &Store, s: &Suggestion, ctx: &Ctx) -> Result<String> {
    let (path, label) = target_path(store, s, ctx)?;
    let current = std::fs::read_to_string(&path).ok();
    let next = s.change.as_ref().unwrap().edit.apply(current.as_deref())?;
    Ok(unified_diff(&label, current.as_deref().unwrap_or(""), &next))
}

pub fn confirm<R: BufRead, W: Write>(input: &mut R, out: &mut W, question: &str) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

pub fn apply<R: BufRead, W: Write>(store: &Store, id: &str, ctx: &Ctx, yes: bool, input: &mut R, out: &mut W, now: DateTime<Utc>) -> Result<bool> {
    let s = suggest::get(store, id)?;
    if s.status != "pending" {
        bail!("{id} is {}; only pending suggestions can be applied", s.status);
    }
    let (path, label) = target_path(store, &s, ctx)?;
    let current = std::fs::read_to_string(&path).ok();
    let next = s.change.as_ref().unwrap().edit.apply(current.as_deref())?;
    if current.as_deref() == Some(next.as_str()) {
        bail!("{label} already has this change");
    }
    write!(out, "{}", s.render())?;
    writeln!(out, "\n{}", unified_diff(&label, current.as_deref().unwrap_or(""), &next))?;
    if !yes && !confirm(input, out, &format!("Write this change to {}?", path.display()))? {
        writeln!(out, "not applied")?;
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, &next).with_context(|| format!("write {}", path.display()))?;
    store.conn().execute(
        "INSERT INTO adoptions(suggestion_id, applied_at, target_path, before_exists, before_hash, after_hash, before_content, after_content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, ts(now), path.to_string_lossy(), current.is_some(), current.as_deref().map(sha), sha(&next), current, next],
    )?;
    suggest::set_status(store, id, "applied", now)?;
    writeln!(out, "applied {id} to {}; undo with: lightning rollback {id}", path.display())?;
    Ok(true)
}

pub fn rollback<R: BufRead, W: Write>(store: &Store, id: &str, yes: bool, force: bool, input: &mut R, out: &mut W, now: DateTime<Utc>) -> Result<bool> {
    let row: Option<(i64, String, bool, Option<String>, String)> = store
        .conn()
        .query_row(
            "SELECT id, target_path, before_exists, before_content, after_hash FROM adoptions
             WHERE suggestion_id = ?1 AND rolled_back_at IS NULL ORDER BY id DESC LIMIT 1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((aid, path, before_exists, before, after_hash)) = row else { bail!("{id} has no applied change to roll back") };
    let path = PathBuf::from(path);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref().map(sha).as_deref() != Some(after_hash.as_str()) && !force {
        bail!("{} changed since {id} was applied; inspect it, then pass --force to restore the earlier version anyway", path.display());
    }
    let restored = if before_exists { before.clone().unwrap_or_default() } else { String::new() };
    writeln!(out, "{}", unified_diff(&path.to_string_lossy(), current.as_deref().unwrap_or(""), &restored))?;
    if !yes && !confirm(input, out, &format!("Restore {}?", path.display()))? {
        writeln!(out, "not rolled back")?;
        return Ok(false);
    }
    if before_exists {
        std::fs::write(&path, restored)?;
    } else if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    store.conn().execute("UPDATE adoptions SET rolled_back_at = ?2 WHERE id = ?1", params![aid, ts(now)])?;
    suggest::set_status(store, id, "rolled_back", now)?;
    writeln!(out, "rolled back {id}")?;
    Ok(true)
}

#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub suggestion_id: String,
    pub kind: String,
    pub applied_at: String,
    pub rolled_back: bool,
    pub verdict: String,
    pub before_n: u64,
    pub after_n: u64,
    pub metric: String,
    pub metric_change: Option<stats::Interval>,
    pub success_diff: Option<stats::Interval>,
    pub summary: String,
}

pub const REVIEW_MIN_N: u64 = 10;
pub const REVIEW_WINDOW_DAYS: i64 = 30;

fn in_window(start: &Option<String>, from: &str, to: &str) -> bool {
    start.as_deref().is_some_and(|s| s >= from && s < to)
}

/// Closed-loop results for every adoption.
pub fn review(store: &Store, segs: &[Segment], runs: &[SubagentRun], now: DateTime<Utc>) -> Result<Vec<Review>> {
    let mut st = store.conn().prepare("SELECT suggestion_id, applied_at, rolled_back_at FROM adoptions ORDER BY id")?;
    let rows: Vec<(String, String, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, applied, rolled) in rows {
        let s = suggest::get(store, &id)?;
        let at = DateTime::parse_from_rfc3339(&applied)?.with_timezone(&Utc);
        let from = ts(at - Duration::days(REVIEW_WINDOW_DAYS));
        let until = rolled.clone().unwrap_or_else(|| ts(now + Duration::days(1)));
        let mut r = Review {
            suggestion_id: id.clone(),
            kind: s.kind.as_str().into(),
            applied_at: applied.clone(),
            rolled_back: rolled.is_some(),
            verdict: "insufficient_data".into(),
            before_n: 0,
            after_n: 0,
            metric: String::new(),
            metric_change: None,
            success_diff: None,
            summary: String::new(),
        };
        match s.kind {
            Kind::Default | Kind::Rule => {
                let scoped = |x: &&Segment| {
                    s.scope.repo_key.as_ref().is_none_or(|k| &x.repo_key == k)
                        && s.scope.task.as_ref().is_none_or(|t| &x.task_type == t)
                        && s.scope.harness.as_ref().is_none_or(|h| &x.source == h)
                        && (s.kind != Kind::Rule || s.scope.pair_from.as_ref().is_none_or(|p| &x.pair() == p))
                };
                let pool: Vec<&Segment> = analysis::comparable(segs).into_iter().filter(scoped).collect();
                let before: Vec<&Segment> = pool.iter().filter(|x| in_window(&x.start, &from, &applied)).copied().collect();
                let after: Vec<&Segment> = pool.iter().filter(|x| in_window(&x.start, &applied, &until)).copied().collect();
                r.before_n = before.len() as u64;
                r.after_n = after.len() as u64;
                if s.kind == Kind::Default {
                    let f = analysis::compare_pairs_by("all", "before", &before, "after", &after, REVIEW_MIN_N, |x| format!("{}|{}", x.task_type, x.size_bucket()));
                    r.metric = format!("{} per segment (after / before)", f.cost_basis);
                    if let Some(d) = &f.diff {
                        r.metric_change = d.cost_ratio.clone();
                        r.success_diff = d.success_diff.clone();
                    }
                    r.verdict = match (&r.metric_change, &r.success_diff) {
                        _ if f.verdict == analysis::Verdict::InsufficientData => "insufficient_data",
                        (_, Some(sd)) if sd.hi < -analysis::SUCCESS_MARGIN => "hurt",
                        (Some(c), _) if c.lo > 1.0 => "hurt",
                        (Some(c), Some(sd)) if c.hi < 1.0 && sd.lo > -analysis::SUCCESS_MARGIN => "helped",
                        _ => "no_clear_effect",
                    }
                    .into();
                } else {
                    let metric = s.scope.metric.clone().unwrap_or_default();
                    r.metric = format!("{metric} rate (after - before)");
                    let rate = |v: &[&Segment]| -> (u64, u64) {
                        let h: Vec<bool> = v
                            .iter()
                            .filter_map(|x| match metric.as_str() {
                                "reread" => (x.reads > 0).then_some(x.rereads > 0),
                                "retry" => (x.tool_calls > 0).then_some(x.retries > 0),
                                "tool_error" => (x.tool_calls > 0).then_some(x.tool_errors > 0),
                                _ => (x.tool_calls > 0).then_some(x.denials > 0),
                            })
                            .collect();
                        (h.iter().filter(|b| **b).count() as u64, h.len() as u64)
                    };
                    let ((k1, n1), (k2, n2)) = (rate(&before), rate(&after));
                    if n1 >= REVIEW_MIN_N && n2 >= REVIEW_MIN_N {
                        r.metric_change = stats::newcombe(k1, n1, k2, n2);
                        let succ = |v: &[&Segment]| {
                            let d: Vec<bool> = v.iter().filter_map(|x| x.success()).collect();
                            (d.iter().filter(|b| **b).count() as u64, d.len() as u64)
                        };
                        let ((s1, d1), (s2, d2)) = (succ(&before), succ(&after));
                        r.success_diff = stats::newcombe(s1, d1, s2, d2);
                        let hurt_quality = r.success_diff.as_ref().is_some_and(|x| x.hi < -analysis::SUCCESS_MARGIN);
                        r.verdict = match &r.metric_change {
                            _ if hurt_quality => "hurt",
                            Some(m) if m.hi < 0.0 => "helped",
                            Some(m) if m.lo > 0.0 => "hurt",
                            _ => "no_clear_effect",
                        }
                        .into();
                    }
                }
            }
            Kind::Subagent => {
                let agent = s.scope.agent_type.clone().unwrap_or_default();
                let pool: Vec<&SubagentRun> = runs.iter().filter(|x| x.agent_type == agent).collect();
                let before: Vec<&SubagentRun> = pool.iter().filter(|x| in_window(&x.start, &from, &applied)).copied().collect();
                let after: Vec<&SubagentRun> = pool.iter().filter(|x| in_window(&x.start, &applied, &until)).copied().collect();
                r.before_n = before.len() as u64;
                r.after_n = after.len() as u64;
                let priced = before.iter().chain(after.iter()).all(|x| x.cost.is_some());
                r.metric = format!("{} per run (after / before)", if priced { "usd" } else { "tokens" });
                if r.before_n >= REVIEW_MIN_N && r.after_n >= REVIEW_MIN_N {
                    let obs = |v: &[&SubagentRun]| -> Vec<stats::Obs> {
                        v.iter().map(|x| stats::Obs { stratum: "all".into(), success: Some(!x.errored), cost: if priced { x.cost.unwrap_or(0.0) } else { x.tokens as f64 } }).collect()
                    };
                    let d = stats::compare(&obs(&before), &obs(&after), analysis::BOOT_REPS, analysis::SEED);
                    r.metric_change = d.cost_ratio.clone();
                    r.success_diff = d.success_diff.clone();
                    r.verdict = match (&d.cost_ratio, &d.success_diff) {
                        (_, Some(sd)) if sd.hi < -analysis::SUCCESS_MARGIN => "hurt",
                        (Some(c), _) if c.lo > 1.0 => "hurt",
                        (Some(c), Some(sd)) if c.hi < 1.0 && sd.lo > -analysis::SUCCESS_MARGIN => "helped",
                        _ => "no_clear_effect",
                    }
                    .into();
                }
            }
            Kind::Hygiene => {
                r.verdict = "not_measured".into();
            }
        }
        r.summary = match r.verdict.as_str() {
            "insufficient_data" => format!("{id}: need {REVIEW_MIN_N}+ segments on each side (before {}, after {})", r.before_n, r.after_n),
            "hurt" if !r.rolled_back => format!("{id}: hurt. Consider: lightning rollback {id}"),
            v => format!("{id}: {}", v.replace('_', " ")),
        };
        out.push(r);
    }
    Ok(out)
}

/// Resolve the repo for `apply`: `--repo`, else the current directory.
pub fn ctx_from_env(repo: Option<&Path>) -> Result<Ctx> {
    let home = crate::paths::home().context("cannot find the home directory")?;
    let repo = match repo {
        Some(r) => Some(r.to_path_buf()),
        None => std::env::current_dir().ok(),
    };
    Ok(Ctx { home, repo })
}
