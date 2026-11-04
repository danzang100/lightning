//! Passive weekly report (lightning-spec 11.4): Markdown and HTML, zero LLM calls.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, Utc};

use crate::analysis::{self, PairKey};
use crate::doctor::DoctorReport;
use crate::render::{self, esc};
use crate::segment::Segment;
use crate::suggest::Suggestion;
use crate::switches::SwitchReport;
use crate::adopt::Review;

pub const COMPARE_DAYS: i64 = 30;

pub struct Inputs<'a> {
    pub now: DateTime<Utc>,
    /// Segments from the last `COMPARE_DAYS` days.
    pub segments: &'a [Segment],
    pub switches: &'a SwitchReport,
    pub pending: &'a [Suggestion],
    pub reviews: &'a [Review],
    pub doctor: &'a DoctorReport,
    pub min_n: u64,
}

pub struct Weekly {
    pub markdown: String,
    pub html: String,
    pub week_of: String,
}

pub fn build(i: &Inputs) -> Weekly {
    let monday = i.now.date_naive() - Duration::days(i.now.weekday().num_days_from_monday() as i64);
    let week_of = monday.format("%Y-%m-%d").to_string();
    let week_start = (i.now - Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let this_week = i.segments.iter().filter(|s| s.start.as_deref().is_some_and(|x| x >= week_start.as_str())).count();
    let repos: std::collections::BTreeSet<&str> = i.segments.iter().map(|s| s.repo_key.as_str()).collect();
    let harnesses: std::collections::BTreeSet<&str> = i.segments.iter().map(|s| s.source.as_str()).collect();
    let health = if i.doctor.warnings.is_empty() { "OK".to_string() } else { format!("{} warning(s)", i.doctor.warnings.len()) };
    let data = format!(
        "Data: {} segments in the last {COMPARE_DAYS} days ({this_week} this week), {} repos, {} harness(es). Parser health: {health}.",
        i.segments.len(),
        repos.len(),
        harnesses.len()
    );
    let cells = analysis::cells(i.segments, PairKey::Pair, true, i.min_n);
    let grid = render::grid(&cells, 5);
    let findings = analysis::findings(i.segments, PairKey::Pair, i.min_n);
    let mut switch_lines: Vec<String> = i
        .switches
        .within_session
        .iter()
        .map(|s| {
            format!(
                "{} -> {}: {} switch(es), cache-hit drop {}, extra cost/turn {} ({} confidence)",
                s.from,
                s.to,
                s.n,
                s.median_cache_hit_drop.map_or("n/a".into(), |v| format!("{v:.0} pts")),
                s.mean_extra_cost_per_turn.map_or("unpriced".into(), |v| format!("${v:.4}")),
                s.confidence
            )
        })
        .collect();
    switch_lines.sort();
    switch_lines.truncate(5);
    let pending: Vec<String> = i.pending.iter().map(|s| format!("{} ({}, {}): {}", s.id, s.kind.as_str(), s.confidence, s.rationale)).collect();
    let reviews: Vec<String> = i.reviews.iter().map(|r| r.summary.clone()).collect();
    let finding_lines: Vec<String> = findings.iter().map(|f| f.summary.clone()).collect();

    let title = format!("Lightning: week of {week_of}");
    let bullets = |v: &[String]| if v.is_empty() { "- none\n".to_string() } else { v.iter().map(|x| format!("- {x}\n")).collect() };
    let mut md = format!("# {title}\n\n{data}\n\n## Pair x task (proxy success, cost/segment, n)\n\n");
    if grid.tasks.is_empty() {
        md.push_str("No segments with a task type yet.\n");
    } else {
        md.push_str(&grid.md());
    }
    md.push_str(&format!("\n## Findings\n\n{}", bullets(&finding_lines)));
    md.push_str(&format!("\n## Switches (last 30 days)\n\n{}", bullets(&switch_lines)));
    md.push_str(&format!("\n## Pending suggestions: {}\n\n{}", pending.len(), bullets(&pending)));
    md.push_str(&format!("\n## Closed loop\n\n{}", bullets(&reviews)));
    md.push_str(&format!("\n## Data quality\n\n{}", bullets(&i.doctor.warnings)));
    md.push_str("\nProxy success is a heuristic estimate from log signals, not a measured pass rate.\n");

    let table_html = if grid.tasks.is_empty() { "<p class=\"muted\">No segments with a task type yet.</p>".to_string() } else { format!("<div class=\"scroll\">{}</div>", grid.html()) };
    let html = render::page(
        &title,
        &data,
        &[
            ("Pair x task (proxy success, cost/segment, n)".into(), table_html),
            ("Findings".into(), render::list_html(&finding_lines)),
            ("Switches (last 30 days)".into(), render::list_html(&switch_lines)),
            (format!("Pending suggestions: {}", pending.len()), render::list_html(&pending)),
            ("Closed loop".into(), render::list_html(&reviews)),
            ("Data quality".into(), render::list_html(&i.doctor.warnings)),
            ("Note".into(), format!("<p class=\"muted\">{}</p>", esc("Proxy success is a heuristic estimate from log signals, not a measured pass rate."))),
        ],
    );
    Weekly { markdown: md, html, week_of }
}

/// Write `weekly-<monday>.md` and `.html` into `dir`.
pub fn write(w: &Weekly, dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let md = dir.join(format!("weekly-{}.md", w.week_of));
    let html = dir.join(format!("weekly-{}.html", w.week_of));
    std::fs::write(&md, &w.markdown)?;
    std::fs::write(&html, &w.html)?;
    Ok((md, html))
}
