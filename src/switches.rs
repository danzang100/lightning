//! Switch analysis (lightning-spec 9.6): what changes when the pair changes mid-stream.
//!
//! Within a session, a switch is a model or effort change between consecutive main-agent turns;
//! the 3 turns after are compared with the 3 before. Across sessions, a switch is the next
//! session in the same repo within 30 minutes running a different pair.
use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::pricing::{PriceTable, Tokens};
use crate::schema::SYNTHETIC_MODEL;
use crate::stats;
use crate::store::Store;

const WINDOW: usize = 3;
const CROSS_GAP_MINUTES: i64 = 30;

#[derive(Debug, Clone, Serialize)]
pub struct SwitchAgg {
    pub from: String,
    pub to: String,
    pub n: u64,
    /// Median drop in cache-hit share (before minus after), in points.
    pub median_cache_hit_drop: Option<f64>,
    /// Median extra cache-write tokens per turn after the switch.
    pub median_cache_write_spike: Option<f64>,
    /// Mean extra cost per turn after the switch, when both sides are priced.
    pub mean_extra_cost_per_turn: Option<f64>,
    /// Share of switches followed by a correction within the next 3 prompts.
    pub correction_rate: Option<f64>,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossAgg {
    pub from: String,
    pub to: String,
    pub n: u64,
    pub correction_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwitchReport {
    pub since: Option<String>,
    pub within_session: Vec<SwitchAgg>,
    pub cross_session: Vec<CrossAgg>,
    pub note: String,
}

struct T {
    session: String,
    repo: String,
    pair: String,
    model: String,
    ts: Option<String>,
    tokens: Tokens,
}

fn share(ts: &[&T]) -> Option<f64> {
    let (mut cr, mut all) = (0i64, 0i64);
    for t in ts {
        let c = t.tokens.cache_read.unwrap_or(0);
        cr += c;
        all += c + t.tokens.input.unwrap_or(0) + t.tokens.cache_write_5m.unwrap_or(0) + t.tokens.cache_write_1h.unwrap_or(0);
    }
    (all > 0).then(|| cr as f64 / all as f64)
}

fn mean_write(ts: &[&T]) -> f64 {
    ts.iter().map(|t| (t.tokens.cache_write_5m.unwrap_or(0) + t.tokens.cache_write_1h.unwrap_or(0)) as f64).sum::<f64>() / ts.len().max(1) as f64
}

fn mean_cost(ts: &[&T], pricing: &PriceTable) -> Option<f64> {
    let mut sum = 0.0;
    for t in ts {
        sum += pricing.cost(&t.model, t.tokens)?;
    }
    Some(sum / ts.len().max(1) as f64)
}

fn confidence(n: u64) -> &'static str {
    match n {
        0..=4 => "low",
        5..=19 => "medium",
        _ => "high",
    }
}

pub fn build(store: &Store, pricing: &PriceTable, since: Option<DateTime<Utc>>) -> Result<SwitchReport> {
    let c = store.conn();
    let cutoff = since.map(|s| s.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let mut by_session: BTreeMap<String, Vec<T>> = BTreeMap::new();
    let mut st = c.prepare(
        "SELECT t.session_id, COALESCE(s.cwd_hash, t.project), COALESCE(t.source, 'claude_code'), t.model, t.effort, t.ts,
                t.input_tokens, t.output_tokens, t.cache_read_tokens, t.cache_write_5m_tokens, t.cache_write_1h_tokens
         FROM turns t LEFT JOIN sessions s ON s.session_id = t.session_id AND s.agent_id = ''
         WHERE t.is_subagent = 0 AND t.model != ?1 AND t.model != '' ORDER BY t.session_id, t.ts, t.turn_id",
    )?;
    let mut rows = st.query([SYNTHETIC_MODEL])?;
    while let Some(r) = rows.next()? {
        let session: String = r.get(0)?;
        let source: String = r.get(2)?;
        let model: String = r.get(3)?;
        let effort: Option<String> = r.get(4)?;
        let ts: Option<String> = r.get(5)?;
        if let (Some(cut), Some(t)) = (&cutoff, &ts)
            && t < cut
        {
            continue;
        }
        let pair = match &effort {
            Some(e) => format!("{source}/{model}@{e}"),
            None => format!("{source}/{model}"),
        };
        by_session.entry(session.clone()).or_default().push(T {
            session,
            repo: r.get(1)?,
            pair,
            model,
            ts,
            tokens: Tokens { input: r.get(6)?, output: r.get(7)?, cache_read: r.get(8)?, cache_write_5m: r.get(9)?, cache_write_1h: r.get(10)? },
        });
    }
    drop(rows);
    drop(st);

    let mut corrections: HashMap<String, Vec<(Option<String>, bool)>> = HashMap::new();
    let mut st = c.prepare("SELECT session_id, ts, class FROM signals WHERE kind = 'prompt' AND agent_id = '' AND class IS NOT NULL ORDER BY session_id, ts, id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?)))? {
        let (s, ts, class) = r?;
        corrections.entry(s).or_default().push((ts, class == "correction"));
    }

    let corrected_after = |session: &str, ts: &Option<String>| -> bool {
        corrections.get(session).is_some_and(|v| v.iter().filter(|(t, _)| t > ts).take(WINDOW).any(|(_, c)| *c))
    };

    #[derive(Default)]
    struct Acc {
        drops: Vec<f64>,
        spikes: Vec<f64>,
        extra: Vec<f64>,
        corrected: u64,
        n: u64,
    }
    let mut within: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for turns in by_session.values() {
        for i in 1..turns.len() {
            if turns[i].pair == turns[i - 1].pair {
                continue;
            }
            let before: Vec<&T> = turns[i.saturating_sub(WINDOW)..i].iter().collect();
            let after: Vec<&T> = turns[i..(i + WINDOW).min(turns.len())].iter().collect();
            let a = within.entry((turns[i - 1].pair.clone(), turns[i].pair.clone())).or_default();
            a.n += 1;
            if let (Some(b), Some(x)) = (share(&before), share(&after)) {
                a.drops.push((b - x) * 100.0);
            }
            a.spikes.push(mean_write(&after) - mean_write(&before));
            if let (Some(b), Some(x)) = (mean_cost(&before, pricing), mean_cost(&after, pricing)) {
                a.extra.push(x - b);
            }
            if corrected_after(&turns[i].session, &turns[i].ts) {
                a.corrected += 1;
            }
        }
    }

    // Cross-session: sessions ordered by start, per repo.
    let mut starts: Vec<(&String, Option<&String>, Option<&String>, &String, String)> = Vec::new();
    for (sid, turns) in &by_session {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in turns {
            *counts.entry(t.pair.split('@').next().unwrap_or(&t.pair)).or_default() += 1;
        }
        let dominant = counts.iter().max_by_key(|(k, v)| (**v, std::cmp::Reverse(**k))).map(|(k, _)| k.to_string()).unwrap_or_default();
        starts.push((&turns[0].repo, turns.first().and_then(|t| t.ts.as_ref()), turns.last().and_then(|t| t.ts.as_ref()), sid, dominant));
    }
    starts.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    let mut cross: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    for w in starts.windows(2) {
        let (prev, next) = (&w[0], &w[1]);
        if prev.0 != next.0 || prev.4 == next.4 {
            continue;
        }
        let parse = |s: Option<&String>| s.and_then(|x| DateTime::parse_from_rfc3339(x).ok());
        let close = matches!((parse(prev.2), parse(next.1)), (Some(e), Some(s)) if (s - e).num_minutes() <= CROSS_GAP_MINUTES && s >= e);
        if !close {
            continue;
        }
        let e = cross.entry((prev.4.clone(), next.4.clone())).or_default();
        e.0 += 1;
        let corrected = corrections.get(next.3.as_str()).is_some_and(|v| v.iter().take(WINDOW).any(|(_, c)| *c));
        e.1 += corrected as u64;
    }

    Ok(SwitchReport {
        since: cutoff,
        within_session: within
            .into_iter()
            .map(|((from, to), a)| SwitchAgg {
                from,
                to,
                n: a.n,
                median_cache_hit_drop: stats::median(&a.drops),
                median_cache_write_spike: stats::median(&a.spikes),
                mean_extra_cost_per_turn: stats::mean(&a.extra),
                correction_rate: (a.n > 0).then(|| a.corrected as f64 / a.n as f64),
                confidence: confidence(a.n).to_string(),
            })
            .collect(),
        cross_session: cross
            .into_iter()
            .map(|((from, to), (n, c))| CrossAgg { from, to, n, correction_rate: (n > 0).then(|| c as f64 / n as f64) })
            .collect(),
        note: "Assumption under test: provider prompt caches do not survive a model switch. The cache-hit drop and cache-write spike measure that cost directly.".to_string(),
    })
}

impl SwitchReport {
    pub fn render(&self) -> String {
        let mut o = String::from("Lightning switches\n\n");
        if self.within_session.is_empty() {
            o.push_str("no within-session switches\n");
        } else {
            o.push_str("within session (3 turns after vs 3 before)\n");
            for s in &self.within_session {
                o.push_str(&format!(
                    "- {} -> {}: n={}, cache-hit drop {}, cache-write spike {}, extra cost/turn {}, corrected after {} ({} confidence)\n",
                    s.from,
                    s.to,
                    s.n,
                    s.median_cache_hit_drop.map_or("n/a".into(), |v| format!("{v:.1} pts")),
                    s.median_cache_write_spike.map_or("n/a".into(), |v| format!("{v:.0} tok")),
                    s.mean_extra_cost_per_turn.map_or("unpriced".into(), |v| format!("${v:.4}")),
                    s.correction_rate.map_or("n/a".into(), |v| format!("{:.0}%", v * 100.0)),
                    s.confidence
                ));
            }
        }
        if !self.cross_session.is_empty() {
            o.push_str("\nacross sessions (same repo, within 30 min)\n");
            for s in &self.cross_session {
                o.push_str(&format!(
                    "- {} -> {}: n={}, corrected early {}\n",
                    s.from,
                    s.to,
                    s.n,
                    s.correction_rate.map_or("n/a".into(), |v| format!("{:.0}%", v * 100.0))
                ));
            }
        }
        o.push_str(&format!("\nnote: {}\n", self.note));
        o
    }
}
