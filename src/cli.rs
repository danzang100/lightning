use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

use crate::adapter::Adapter;
use crate::adapter::claude_code::ClaudeCode;
use crate::adapter::codex::{self, Codex};
use crate::adapter::opencode::{self, OpenCode};
use crate::analysis::{self, PairKey};
use crate::label;
use crate::pricing::PriceTable;
use crate::render;
use crate::segment::{self, Segment, SegmentOptions};
use crate::store::Store;
use crate::suggest::{self, Kind};
use crate::{adopt, doctor, export, ingest, mcp, paths, report, switches, weekly};

#[derive(Parser, Debug)]
#[command(
    name = "lightning",
    version,
    about = "Which harness + model pair works best for which kind of work, from your own local agent logs"
)]
pub struct Cli {
    /// SQLite database path (default: platform data dir).
    #[arg(long, global = true, env = "LIGHTNING_DB")]
    pub db: Option<PathBuf>,
    /// Pricing override file (default: <data dir>/lightning/pricing.toml).
    #[arg(long, global = true, env = "LIGHTNING_PRICING")]
    pub pricing: Option<PathBuf>,
    /// Minimum decided segments per cell before comparing.
    #[arg(long, global = true, default_value_t = analysis::DEFAULT_MIN_N)]
    pub min_n: u64,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Read transcripts and store normalized rows. Rows outlive the harness's own retention.
    Ingest {
        /// claude, codex, opencode or all (sources whose logs are missing are skipped).
        #[arg(long, default_value = "all")]
        source: String,
        /// Log root for a single source (default: the harness's usual location).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Print nothing on success (for hooks).
        #[arg(long)]
        quiet: bool,
    },
    /// Tables by model, repo or session (descriptive), or by pair / pair+effort / task (comparative).
    Report {
        #[arg(long)]
        since: Option<String>,
        /// model, repo, session, pair, pair+effort, task
        #[arg(long, default_value = "task")]
        by: String,
        /// table, md, json, html
        #[arg(long, default_value = "table")]
        format: String,
        #[arg(long)]
        exclude_subagents: bool,
        #[arg(long)]
        include_synthetic: bool,
    },
    /// Compare two pairs (harness/model, harness/model@effort, or a model name).
    Compare {
        pair_a: String,
        pair_b: String,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// What model or harness switches cost (cache loss, token spikes, corrections).
    Switches {
        #[arg(long, default_value = "30d")]
        since: String,
        #[arg(long)]
        json: bool,
    },
    /// List segments with their task type, pair and proxy outcome.
    Segments {
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Hand-label segments and check heuristic agreement (the go/no-go gate).
    Label {
        #[command(subcommand)]
        action: Option<LabelAction>,
    },
    /// Propose evidence-backed changes (diffs you approve with `apply`).
    Suggest {
        /// default, subagent, rule or hygiene (repeatable; default all).
        #[arg(long = "type")]
        kinds: Vec<String>,
        /// List stored suggestions without recomputing.
        #[arg(long)]
        list: bool,
        /// Show one suggestion, with its diff when the target can be resolved.
        #[arg(long)]
        show: Option<String>,
        /// Dismiss a suggestion so it is not proposed again.
        #[arg(long)]
        dismiss: Option<String>,
        /// Repo root for previewing repo-level diffs (default: current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Show a suggestion's diff, confirm, write it and record hashes.
    Apply {
        id: String,
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Restore the file an applied suggestion changed.
    Rollback {
        id: String,
        #[arg(long)]
        yes: bool,
        /// Restore even if the file changed since apply.
        #[arg(long)]
        force: bool,
    },
    /// Closed loop: did each applied suggestion help?
    Review {
        #[arg(long)]
        json: bool,
    },
    /// Parser health per harness, retention and pricing warnings.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Export normalized records as JSONL (OTel GenAI names where they exist).
    Export {
        #[arg(long, default_value = "jsonl")]
        format: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Ingest everything available, then write this week's report (md + html).
    Weekly {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        no_ingest: bool,
    },
    /// Read-only MCP server on stdio.
    Mcp,
}

#[derive(Subcommand, Debug)]
pub enum LabelAction {
    /// Label a balanced sample interactively.
    Next {
        #[arg(long, default_value_t = 40)]
        sample: usize,
    },
    /// Show a segment with its opening prompt (read from the source, not stored).
    Show { segment: String },
    /// Record a label non-interactively.
    Set {
        segment: String,
        #[arg(long)]
        task: Option<String>,
        /// success, failure or unknown
        #[arg(long)]
        outcome: Option<String>,
    },
    /// Heuristic vs hand-label agreement against the go/no-go thresholds.
    Agreement {
        #[arg(long)]
        json: bool,
    },
}

fn since_of(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    s.map(|x| report::parse_since(x, Utc::now())).transpose()
}

type Sources = Vec<(Box<dyn Adapter>, Option<PathBuf>)>;

/// Adapters to run with their roots. `--root` without `--source` means Claude Code logs.
fn sources(source: &str, root: Option<PathBuf>) -> Result<Sources> {
    let all: Vec<(&str, Box<dyn Adapter>, Option<PathBuf>)> = vec![
        ("claude", Box::new(ClaudeCode), std::env::var_os("LIGHTNING_CLAUDE_ROOT").map(PathBuf::from).or_else(paths::default_claude_root)),
        ("codex", Box::new(Codex), std::env::var_os("LIGHTNING_CODEX_ROOT").map(PathBuf::from).or_else(codex::default_root)),
        ("opencode", Box::new(OpenCode), std::env::var_os("LIGHTNING_OPENCODE_ROOT").map(PathBuf::from).or_else(opencode::default_root)),
    ];
    let source = if source == "all" && root.is_some() { "claude" } else { source };
    if source == "all" {
        return Ok(all.into_iter().map(|(_, a, r)| (a, r)).collect());
    }
    let (_, a, r) = all.into_iter().find(|(n, _, _)| *n == source).with_context(|| format!("unknown source {source:?} (claude, codex, opencode, all)"))?;
    Ok(vec![(a, root.or(r))])
}

struct Env {
    db: PathBuf,
    pricing: PriceTable,
    min_n: u64,
}

impl Env {
    fn store(&self) -> Result<Store> {
        Store::open(&self.db)
    }

    fn segments(&self, store: &Store, since: Option<DateTime<Utc>>) -> Result<Vec<Segment>> {
        segment::build(store, &self.pricing, &SegmentOptions { since })
    }
}

pub fn run<W: Write>(cli: Cli, out: &mut W) -> Result<()> {
    let stdin = std::io::stdin();
    run_with_input(cli, &mut stdin.lock(), out)
}

fn ingest_all<W: Write>(env: &Env, source: &str, root: Option<PathBuf>, out: &mut W, json: bool, quiet: bool) -> Result<Store> {
    let explicit = source != "all" || root.is_some();
    let mut total = ingest::IngestSummary::default();
    let mut used = Vec::new();
    // Check every root before the database is created, so a bad --db never lands in a log dir.
    let mut todo = Vec::new();
    for (adapter, root) in sources(source, root)? {
        let Some(root) = root else { continue };
        let present = if adapter.name() == "opencode" { adapter.detect(&root) || root.is_file() } else { root.is_dir() };
        if !present {
            if explicit {
                bail!("{} logs not found at {}", adapter.name(), root.display());
            }
            continue;
        }
        let scan_root = if root.is_file() { root.parent().unwrap_or(Path::new(".")).to_path_buf() } else { root.clone() };
        ingest::ensure_db_outside_root(&env.db, &scan_root)?;
        todo.push((adapter, scan_root));
    }
    let mut store = env.store()?;
    for (adapter, scan_root) in todo {
        let s = ingest::ingest(&mut store, adapter.as_ref(), &scan_root)?;
        total.merge(s);
        used.push(adapter.name());
    }
    total.sources = used.iter().map(|s| s.to_string()).collect();
    if quiet {
        return Ok(store);
    }
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&total)?)?;
    } else {
        if used.is_empty() {
            writeln!(out, "no agent logs found (looked for Claude Code, Codex and OpenCode in their usual places)")?;
        }
        write!(out, "{}", total.render())?;
    }
    Ok(store)
}

pub fn run_with_input<R: BufRead, W: Write>(cli: Cli, input: &mut R, out: &mut W) -> Result<()> {
    let db = cli.db.clone().unwrap_or_else(paths::default_db);
    let pricing = PriceTable::load(Some(&cli.pricing.clone().unwrap_or_else(paths::default_pricing_override)))?;
    let env = Env { db, pricing, min_n: cli.min_n };
    let now = Utc::now();
    match cli.command {
        Command::Ingest { source, root, json, quiet } => {
            ingest_all(&env, &source, root, out, json, quiet)?;
            Ok(())
        }
        Command::Report { since, by, format, exclude_subagents, include_synthetic } => {
            let since = since_of(since.as_deref())?;
            let store = env.store()?;
            let key = match by.as_str() {
                "pair" => Some((PairKey::Pair, false)),
                "pair+effort" => Some((PairKey::PairEffort, false)),
                "task" => Some((PairKey::Pair, true)),
                _ => None,
            };
            let Some((key, by_task)) = key else {
                let by = report::GroupBy::parse(&by)?;
                if format == "html" {
                    bail!("--format html is available for --by pair, pair+effort and task");
                }
                let format = report::Format::parse(&format)?;
                let opts = report::ReportOptions { since, by, include_subagents: !exclude_subagents, include_synthetic };
                write!(out, "{}", report::build(&store, &env.pricing, &opts)?.render(format)?)?;
                return Ok(());
            };
            let segs = env.segments(&store, since)?;
            let cells = analysis::cells(&segs, key, by_task, env.min_n);
            let findings = analysis::findings(&segs, key, env.min_n);
            let heading = format!(
                "Lightning report: by {by}{} | {} segments | min n {} | proxy success (95% Wilson), cost per segment",
                since.map(|s| format!(", since {}", s.format("%Y-%m-%d"))).unwrap_or_default(),
                segs.len(),
                env.min_n
            );
            match format.as_str() {
                "json" => writeln!(out, "{}", serde_json::to_string_pretty(&serde_json::json!({"heading": heading, "cells": cells, "findings": findings}))?)?,
                "html" => {
                    let grid = render::grid(&cells, 8);
                    let lines: Vec<String> = findings.iter().map(|f| f.summary.clone()).collect();
                    write!(out, "{}", render::page("Lightning report", &heading, &[("Pair x task".into(), format!("<div class=\"scroll\">{}</div>", grid.html())), ("Findings".into(), render::list_html(&lines))]))?;
                }
                "md" => {
                    let grid = render::grid(&cells, 8);
                    write!(out, "**{heading}**\n\n{}\n**Findings**\n\n{}", grid.md(), render::findings_text(&findings))?;
                }
                "table" => {
                    let grid = render::grid(&cells, 8);
                    write!(out, "{heading}\n\n{}\nfindings\n{}", grid.text(), render::findings_text(&findings))?;
                }
                other => bail!("unknown --format {other:?} (table, md, json, html)"),
            }
            Ok(())
        }
        Command::Compare { pair_a, pair_b, task, since, json } => {
            let store = env.store()?;
            let segs = env.segments(&store, since_of(since.as_deref())?)?;
            let (ka, a) = analysis::resolve_pair(&segs, &pair_a)?;
            let (kb, b) = analysis::resolve_pair(&segs, &pair_b)?;
            let pool: Vec<&Segment> = analysis::comparable(&segs).into_iter().filter(|s| task.as_ref().is_none_or(|t| &s.task_type == t)).collect();
            let sa: Vec<&Segment> = pool.iter().filter(|s| ka.of(s) == a).copied().collect();
            let sb: Vec<&Segment> = pool.iter().filter(|s| kb.of(s) == b).copied().collect();
            let label = task.clone().unwrap_or_else(|| "all tasks".into());
            let f = if task.is_some() {
                analysis::compare_pairs(&label, &a, &sa, &b, &sb, env.min_n)
            } else {
                analysis::compare_pairs_by(&label, &a, &sa, &b, &sb, env.min_n, |s| format!("{}|{}", s.task_type, s.stratum()))
            };
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&f)?)?;
            } else {
                let pct = |x: Option<f64>| x.map_or("n/a".into(), |v| format!("{:.0}%", v * 100.0));
                writeln!(out, "{}\n", f.summary)?;
                writeln!(out, "{a}: n={} decided, proxy success {}", f.n_a, pct(f.success_a))?;
                writeln!(out, "{b}: n={} decided, proxy success {}", f.n_b, pct(f.success_b))?;
                if let Some(d) = &f.diff {
                    writeln!(out, "success difference (B - A, 90%): {}", analysis::interval_text(&d.success_diff, 100.0, " pts"))?;
                    writeln!(out, "{} ratio (B / A, 90%): {}", f.cost_basis, analysis::interval_text(&d.cost_ratio, 1.0, ""))?;
                    writeln!(out, "matched strata: {} ({})", d.strata_used, if d.matched { "matched" } else { "unmatched" })?;
                }
                writeln!(out, "days alternating both pairs: {}", f.alternation_days)?;
                writeln!(out, "verdict: {:?} ({} confidence)\nwould change: {}", f.verdict, f.confidence, f.would_change)?;
            }
            Ok(())
        }
        Command::Switches { since, json } => {
            let store = env.store()?;
            let rep = switches::build(&store, &env.pricing, since_of(Some(&since))?)?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&rep)?)?;
            } else {
                write!(out, "{}", rep.render())?;
            }
            Ok(())
        }
        Command::Segments { since, limit, json } => {
            let store = env.store()?;
            let mut segs = env.segments(&store, since_of(since.as_deref())?)?;
            segs.reverse();
            segs.truncate(limit);
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&segs)?)?;
            } else {
                for s in &segs {
                    writeln!(
                        out,
                        "{}  {}  {:<14} {:<40} {:<14} {:<15} turns {:>3}  {}",
                        &s.segment_id[..8],
                        s.start.as_deref().map(|x| &x[..16.min(x.len())]).unwrap_or("?"),
                        s.repo.chars().take(14).collect::<String>(),
                        if s.mixed { format!("{} (mixed)", s.pair()) } else { s.pair() },
                        s.task_type,
                        s.outcome,
                        s.turns,
                        render::money_or_tokens(s.cost.or(Some(s.total_tokens() as f64)), if s.cost.is_some() { "usd" } else { "tokens" })
                    )?;
                }
            }
            Ok(())
        }
        Command::Label { action } => {
            let store = env.store()?;
            let segs = env.segments(&store, None)?;
            match action.unwrap_or(LabelAction::Next { sample: 40 }) {
                LabelAction::Next { sample } => {
                    label::interactive(&store, &segs, sample, input, out, now)?;
                }
                LabelAction::Show { segment } => write!(out, "{}", label::describe(&store, label::find(&segs, &segment)?)?)?,
                LabelAction::Set { segment, task, outcome } => {
                    let s = label::find(&segs, &segment)?;
                    label::set(&store, &s.segment_id, task.as_deref(), outcome.as_deref(), now)?;
                    writeln!(out, "labeled {}", s.segment_id)?;
                }
                LabelAction::Agreement { json } => {
                    let a = label::agreement(&store, &segs)?;
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&a)?)?;
                    } else {
                        write!(out, "{}", a.render())?;
                    }
                }
            }
            Ok(())
        }
        Command::Suggest { kinds, list, show, dismiss, repo, json } => {
            let store = env.store()?;
            if let Some(id) = dismiss {
                suggest::set_status(&store, &id, "dismissed", now)?;
                writeln!(out, "dismissed {id}")?;
                return Ok(());
            }
            if let Some(id) = show {
                let s = suggest::get(&store, &id)?;
                if json {
                    writeln!(out, "{}", serde_json::to_string_pretty(&s)?)?;
                    return Ok(());
                }
                write!(out, "{}", s.render())?;
                writeln!(out, "Evidence: {}", serde_json::to_string_pretty(&s.evidence)?)?;
                if s.change.is_some() {
                    match adopt::preview(&store, &s, &adopt::ctx_from_env(repo.as_deref())?) {
                        Ok(d) => writeln!(out, "\n{d}")?,
                        Err(e) => writeln!(out, "\n(diff not shown: {e})")?,
                    }
                }
                return Ok(());
            }
            let stored = if list {
                suggest::list(&store, None)?
            } else {
                let kinds: Vec<Kind> = if kinds.is_empty() {
                    vec![Kind::Default, Kind::Subagent, Kind::Rule, Kind::Hygiene]
                } else {
                    kinds.iter().map(|k| Kind::parse(k)).collect::<Result<_>>()?
                };
                let segs = env.segments(&store, Some(now - chrono::Duration::days(90)))?;
                let runs = suggest::subagent_runs(&store, &env.pricing)?;
                suggest::save(&store, suggest::generate(&segs, &runs, &kinds, env.min_n, now), now)?;
                suggest::list(&store, Some("pending"))?
            };
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&stored)?)?;
            } else if stored.is_empty() {
                writeln!(out, "no suggestions: nothing in your data clears the evidence bar yet (min n {})", env.min_n)?;
            } else {
                for s in &stored {
                    writeln!(out, "{}", s.render())?;
                }
            }
            Ok(())
        }
        Command::Apply { id, repo, yes } => {
            let store = env.store()?;
            adopt::apply(&store, &id, &adopt::ctx_from_env(repo.as_deref())?, yes, input, out, now)?;
            Ok(())
        }
        Command::Rollback { id, yes, force } => {
            let store = env.store()?;
            adopt::rollback(&store, &id, yes, force, input, out, now)?;
            Ok(())
        }
        Command::Review { json } => {
            let store = env.store()?;
            let segs = env.segments(&store, None)?;
            let runs = suggest::subagent_runs(&store, &env.pricing)?;
            let r = adopt::review(&store, &segs, &runs, now)?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&r)?)?;
            } else if r.is_empty() {
                writeln!(out, "no applied suggestions yet")?;
            } else {
                for x in &r {
                    writeln!(
                        out,
                        "{}  [{}] applied {}{}: before n={}, after n={}; {} {}; success diff {}",
                        x.summary,
                        x.kind,
                        x.applied_at,
                        if x.rolled_back { " (rolled back)" } else { "" },
                        x.before_n,
                        x.after_n,
                        x.metric,
                        analysis::interval_text(&x.metric_change, 1.0, ""),
                        analysis::interval_text(&x.success_diff, 100.0, " pts")
                    )?;
                }
            }
            Ok(())
        }
        Command::Doctor { json } => {
            let store = env.store()?;
            let rep = doctor::build_with(&store, &env.pricing, now, &doctor::DoctorEnv::detect())?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&rep)?)?;
            } else {
                write!(out, "{}", rep.render())?;
            }
            Ok(())
        }
        Command::Export { format, out: path } => {
            if format != "jsonl" {
                bail!("unknown --format {format:?} (jsonl)");
            }
            let store = env.store()?;
            let segs = env.segments(&store, None)?;
            match path {
                Some(p) => {
                    let mut f = std::io::BufWriter::new(std::fs::File::create(&p).with_context(|| format!("create {}", p.display()))?);
                    let n = export::jsonl(&store, &segs, &mut f)?;
                    writeln!(out, "wrote {n} records to {}", p.display())?;
                }
                None => {
                    export::jsonl(&store, &segs, out)?;
                }
            }
            Ok(())
        }
        Command::Weekly { out: dir, no_ingest } => {
            let store = if no_ingest { env.store()? } else { ingest_all(&env, "all", None, out, false, true)? };
            let since = now - chrono::Duration::days(weekly::COMPARE_DAYS);
            let segs = env.segments(&store, Some(since))?;
            let sw = switches::build(&store, &env.pricing, Some(since))?;
            let runs = suggest::subagent_runs(&store, &env.pricing)?;
            suggest::save(&store, suggest::generate(&segs, &runs, &[Kind::Default, Kind::Subagent, Kind::Rule, Kind::Hygiene], env.min_n, now), now)?;
            let pending = suggest::list(&store, Some("pending"))?;
            let all_segs = env.segments(&store, None)?;
            let reviews = adopt::review(&store, &all_segs, &runs, now)?;
            let doc = doctor::build_with(&store, &env.pricing, now, &doctor::DoctorEnv::detect())?;
            let w = weekly::build(&weekly::Inputs { now, segments: &segs, switches: &sw, pending: &pending, reviews: &reviews, doctor: &doc, min_n: env.min_n });
            let dir = dir.unwrap_or_else(|| paths::data_dir().join("reports"));
            let (md, html) = weekly::write(&w, &dir)?;
            writeln!(out, "wrote {}\nwrote {}", md.display(), html.display())?;
            Ok(())
        }
        Command::Mcp => {
            let server = mcp::Server { store: env.store()?, pricing: env.pricing, min_n: env.min_n };
            server.serve(input, out)
        }
    }
}
