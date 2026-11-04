//! Parser and data health per source, retention and pricing (lightning-spec 11.1, 12).
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::pricing::{PriceTable, Tokens};
use crate::schema::SYNTHETIC_MODEL;
use crate::store::Store;

/// Warn when the oldest transcript is this close to the harness's deletion age.
const RETENTION_MARGIN_DAYS: i64 = 5;
const PRICING_STALE_DAYS: i64 = 90;

#[derive(Debug, Serialize)]
pub struct VersionPresence {
    pub source: String,
    pub version: String,
    pub turns: i64,
    pub with_usage: i64,
    pub with_cache_read: i64,
    pub with_cache_write: i64,
    pub with_stop_reason: i64,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    /// Turns per harness.
    pub sources: Vec<(String, i64)>,
    pub files: i64,
    /// Files whose transcript no longer exists; their rows are kept (the archive).
    pub archived_files: i64,
    pub sessions: i64,
    pub turns: i64,
    pub tool_calls: i64,
    pub bad_lines: i64,
    pub partial_tail_files: i64,
    /// Line types the adapter does not consume (all ignored in M0), with counts.
    pub other_line_types: Vec<(String, i64)>,
    pub versions: Vec<VersionPresence>,
    pub oldest_transcript: Option<String>,
    pub newest_transcript: Option<String>,
    pub pricing_version: String,
    pub unpriced_models: Vec<String>,
    pub warnings: Vec<String>,
}

/// Facts about the machine that doctor checks against. `detect` reads them; tests pass fixed ones.
#[derive(Debug, Clone)]
pub struct DoctorEnv {
    /// Claude Code `cleanupPeriodDays` (30 when unset).
    pub claude_cleanup_days: i64,
    /// Whether to stat source files to count archived ones.
    pub check_files: bool,
}

impl Default for DoctorEnv {
    fn default() -> Self {
        Self { claude_cleanup_days: 30, check_files: true }
    }
}

impl DoctorEnv {
    pub fn detect() -> Self {
        let days = crate::paths::home()
            .map(|h| h.join(".claude").join("settings.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("cleanupPeriodDays").and_then(serde_json::Value::as_i64));
        Self { claude_cleanup_days: days.unwrap_or(30), check_files: true }
    }
}

pub fn build(store: &Store, pricing: &PriceTable, now: DateTime<Utc>) -> Result<DoctorReport> {
    build_with(store, pricing, now, &DoctorEnv::default())
}

pub fn build_with(store: &Store, pricing: &PriceTable, now: DateTime<Utc>, env: &DoctorEnv) -> Result<DoctorReport> {
    let c = store.conn();
    let one = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0)) };
    let files = store.count("files")?;
    let mut sources = Vec::new();
    let mut st = c.prepare("SELECT COALESCE(source, 'claude_code'), COUNT(*) FROM turns GROUP BY 1 ORDER BY 1")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        sources.push(r?);
    }
    let mut archived_files = 0;
    if env.check_files {
        let mut st = c.prepare("SELECT path FROM files")?;
        for p in st.query_map([], |r| r.get::<_, String>(0))? {
            if !std::path::Path::new(&p?).exists() {
                archived_files += 1;
            }
        }
    }
    let turns = store.count("turns")?;
    let tool_calls = store.count("tool_calls")?;
    let sessions = one("SELECT COUNT(*) FROM (SELECT session_id FROM turns UNION SELECT session_id FROM sessions)")?;
    let bad_lines = one("SELECT SUM(bad_lines) FROM files")?;
    let partial_tail_files = one("SELECT SUM(partial_tail) FROM files")?;

    let mut other_line_types = Vec::new();
    let mut st = c.prepare("SELECT type, SUM(count) FROM unknown_types GROUP BY type ORDER BY SUM(count) DESC, type")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        other_line_types.push(r?);
    }

    let mut versions = Vec::new();
    let mut st = c.prepare(
        "SELECT COALESCE(source, 'claude_code'), COALESCE(source_version, '(unknown)'), COUNT(*), COUNT(input_tokens),
                COUNT(cache_read_tokens), COUNT(cache_write_5m_tokens), COUNT(stop_reason)
         FROM turns WHERE model != ?1 GROUP BY 1, 2 ORDER BY 1, 2",
    )?;
    for r in st.query_map([SYNTHETIC_MODEL], |r| {
        Ok(VersionPresence {
            source: r.get(0)?,
            version: r.get(1)?,
            turns: r.get(2)?,
            with_usage: r.get(3)?,
            with_cache_read: r.get(4)?,
            with_cache_write: r.get(5)?,
            with_stop_reason: r.get(6)?,
        })
    })? {
        versions.push(r?);
    }

    let (oldest, newest): (Option<String>, Option<String>) =
        c.query_row("SELECT MIN(started_at), MAX(ended_at) FROM sessions", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let oldest_claude: Option<String> =
        c.query_row("SELECT MIN(started_at) FROM sessions WHERE source = 'claude_code'", [], |r| r.get(0))?;

    let mut models = Vec::new();
    let mut st = c.prepare("SELECT DISTINCT model FROM turns WHERE model != ?1 AND model != '' ORDER BY model")?;
    for r in st.query_map([SYNTHETIC_MODEL], |r| r.get::<_, String>(0))? {
        models.push(r?);
    }
    let probe = Tokens { input: Some(1), output: Some(1), cache_read: Some(1), cache_write_5m: Some(1), cache_write_1h: Some(1) };
    let unpriced_models: Vec<String> = models.into_iter().filter(|m| pricing.cost(m, probe).is_none()).collect();

    let mut warnings = Vec::new();
    if bad_lines > 0 {
        warnings.push(format!("{bad_lines} malformed line(s) skipped; if this grows after a harness update the format may have changed"));
    }
    if !unpriced_models.is_empty() {
        warnings.push(format!(
            "{} model(s) have no complete price and are reported as unpriced: {}",
            unpriced_models.len(),
            unpriced_models.join(", ")
        ));
    }
    match pricing.age_days(now.date_naive()) {
        Some(d) if d > PRICING_STALE_DAYS => {
            warnings.push(format!("pricing table is {d} days old (version {}); check it against the provider's pricing page", pricing.version))
        }
        None => warnings.push(format!("pricing table version {:?} is not a YYYY-MM-DD date", pricing.version)),
        _ => {}
    }
    if let Some(o) = oldest_claude.as_deref().and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        let age = (now - o.with_timezone(&Utc)).num_days();
        let limit = env.claude_cleanup_days;
        if age > limit - RETENTION_MARGIN_DAYS {
            warnings.push(format!(
                "oldest ingested session started {age} days ago; Claude Code deletes transcripts after {limit} days (cleanupPeriodDays), so the source files may disappear. Run ingest regularly: the database keeps the normalized rows"
            ));
        }
    }
    if other_line_types.iter().any(|(t, _)| t == crate::adapter::opencode::UNSUPPORTED_V2) {
        warnings.push("the OpenCode database uses the newer session_v2 layout, which this version cannot read yet".to_string());
    }

    Ok(DoctorReport {
        sources,
        files,
        archived_files,
        sessions,
        turns,
        tool_calls,
        bad_lines,
        partial_tail_files,
        other_line_types,
        versions,
        oldest_transcript: oldest,
        newest_transcript: newest,
        pricing_version: pricing.version.clone(),
        unpriced_models,
        warnings,
    })
}

fn pct(n: i64, d: i64) -> String {
    if d == 0 { "n/a".into() } else { format!("{:.0}%", n as f64 * 100.0 / d as f64) }
}

impl DoctorReport {
    pub fn render(&self) -> String {
        let mut o = String::from("Lightning doctor\n\n");
        o.push_str(&format!(
            "data: {} files, {} sessions, {} turns, {} tool calls\n",
            self.files, self.sessions, self.turns, self.tool_calls
        ));
        if !self.sources.is_empty() {
            let v: Vec<String> = self.sources.iter().map(|(s, n)| format!("{s}={n}")).collect();
            o.push_str(&format!("turns by harness: {}\n", v.join(", ")));
        }
        if self.archived_files > 0 {
            o.push_str(&format!("archive: {} file(s) no longer exist at the source; their rows are kept\n", self.archived_files));
        }
        o.push_str(&format!(
            "parse: {} malformed lines, {} file(s) ending in a partial line\n",
            self.bad_lines, self.partial_tail_files
        ));
        o.push_str(&format!(
            "range: {} .. {}\n",
            self.oldest_transcript.as_deref().unwrap_or("n/a"),
            self.newest_transcript.as_deref().unwrap_or("n/a")
        ));
        if self.other_line_types.is_empty() {
            o.push_str("other line types: none\n");
        } else {
            let v: Vec<String> = self.other_line_types.iter().map(|(t, n)| format!("{t}={n}")).collect();
            o.push_str(&format!("other line types (ignored): {}\n", v.join(", ")));
        }
        o.push_str("\nfield presence by harness version (non-synthetic turns)\n");
        o.push_str("harness      version            turns  usage  cache_read  cache_write  stop_reason\n");
        for v in &self.versions {
            o.push_str(&format!(
                "{:<12} {:<16} {:>7} {:>6} {:>11} {:>12} {:>12}\n",
                v.source,
                v.version,
                v.turns,
                pct(v.with_usage, v.turns),
                pct(v.with_cache_read, v.turns),
                pct(v.with_cache_write, v.turns),
                pct(v.with_stop_reason, v.turns)
            ));
        }
        o.push_str(&format!("\npricing: version {}", self.pricing_version));
        if self.unpriced_models.is_empty() {
            o.push_str(", all ingested models priced\n");
        } else {
            o.push_str(&format!(", unpriced: {}\n", self.unpriced_models.join(", ")));
        }
        if self.warnings.is_empty() {
            o.push_str("\nno warnings\n");
        } else {
            o.push_str("\nwarnings\n");
            for w in &self.warnings {
                o.push_str(&format!("- {w}\n"));
            }
        }
        o
    }
}
