//! Pair x task tables and findings as text, Markdown and self-contained HTML.
use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::{Cell, Finding};

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn money_or_tokens(v: Option<f64>, basis: &str) -> String {
    match (v, basis) {
        (None, _) => "n/a".into(),
        (Some(x), "usd") if x > 0.0 && x < 0.01 => "<$0.01".into(),
        (Some(x), "usd") => format!("${x:.2}"),
        (Some(x), _) if x >= 1e6 => format!("{:.1}M tok", x / 1e6),
        (Some(x), _) if x >= 1e3 => format!("{:.0}k tok", x / 1e3),
        (Some(x), _) => format!("{x:.0} tok"),
    }
}

fn cell_text(c: &Cell) -> String {
    if !c.sufficient {
        return format!("insufficient (n={})", c.decided);
    }
    format!(
        "{} {} (n={})",
        c.success_rate.map_or("n/a".into(), |v| format!("{:.0}%", v * 100.0)),
        money_or_tokens(c.mean_cost, &c.cost_basis),
        c.decided
    )
}

/// Rows = pairs, columns = task types with the most data (at most `max_cols`).
pub struct Grid {
    pub tasks: Vec<String>,
    pub rows: Vec<(String, Vec<String>)>,
}

pub fn grid(cells: &[Cell], max_cols: usize) -> Grid {
    let mut per_task: BTreeMap<&str, u64> = BTreeMap::new();
    for c in cells {
        *per_task.entry(c.task.as_str()).or_default() += c.segments;
    }
    let mut tasks: Vec<(&str, u64)> = per_task.into_iter().filter(|(t, _)| *t != "unknown").collect();
    tasks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let tasks: Vec<String> = tasks.into_iter().take(max_cols).map(|(t, _)| t.to_string()).collect();
    let pairs: BTreeSet<&str> = cells.iter().map(|c| c.pair.as_str()).collect();
    let rows = pairs
        .into_iter()
        .map(|p| {
            let row = tasks
                .iter()
                .map(|t| cells.iter().find(|c| c.pair == p && &c.task == t).map(cell_text).unwrap_or_else(|| "-".into()))
                .collect();
            (p.to_string(), row)
        })
        .collect();
    Grid { tasks, rows }
}

impl Grid {
    pub fn text(&self) -> String {
        let mut w = vec![self.rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4)];
        for (i, t) in self.tasks.iter().enumerate() {
            w.push(self.rows.iter().map(|r| r.1[i].len()).max().unwrap_or(0).max(t.len()));
        }
        let line = |first: &str, rest: &[String]| {
            let mut l = format!("{first:<w0$}", w0 = w[0]);
            for (i, c) in rest.iter().enumerate() {
                l.push_str(&format!("  {c:<wi$}", wi = w[i + 1]));
            }
            l.trim_end().to_string() + "\n"
        };
        let mut o = line("pair", &self.tasks);
        for (p, cells) in &self.rows {
            o.push_str(&line(p, cells));
        }
        o
    }

    pub fn md(&self) -> String {
        let mut o = format!("| pair | {} |\n|---|{}\n", self.tasks.join(" | "), "---|".repeat(self.tasks.len()));
        for (p, cells) in &self.rows {
            o.push_str(&format!("| {} | {} |\n", p, cells.join(" | ")));
        }
        o
    }

    pub fn html(&self) -> String {
        let mut o = String::from("<table><thead><tr><th>pair</th>");
        for t in &self.tasks {
            o.push_str(&format!("<th>{}</th>", esc(t)));
        }
        o.push_str("</tr></thead><tbody>");
        for (p, cells) in &self.rows {
            o.push_str(&format!("<tr><th scope=\"row\">{}</th>", esc(p)));
            for c in cells {
                let cls = if c.starts_with("insufficient") || c == "-" { " class=\"muted\"" } else { "" };
                o.push_str(&format!("<td{cls}>{}</td>", esc(c)));
            }
            o.push_str("</tr>");
        }
        o.push_str("</tbody></table>");
        o
    }
}

pub fn findings_text(f: &[Finding]) -> String {
    if f.is_empty() {
        return "no pair has enough decided segments in any task type to compare yet\n".into();
    }
    let mut o = String::new();
    for (i, x) in f.iter().enumerate() {
        o.push_str(&format!("{}. {}\n   would change: {}\n", i + 1, x.summary, x.would_change));
    }
    o
}

pub const CSS: &str = ":root{--bg:#fbfbfa;--fg:#1d1d1b;--muted:#77756f;--line:#e4e2dc;--accent:#b4570f}
@media (prefers-color-scheme:dark){:root{--bg:#161614;--fg:#ecebe6;--muted:#9a978f;--line:#33322e;--accent:#f0a35e}}
body{background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,Segoe UI,sans-serif;margin:0;padding:24px 16px}
main{max-width:960px;margin:0 auto}h1{font-size:22px;margin:0 0 4px}h2{font-size:16px;margin:28px 0 8px;color:var(--accent)}
.sub{color:var(--muted);margin:0 0 16px}.scroll{overflow-x:auto}table{border-collapse:collapse;font-variant-numeric:tabular-nums;font-size:14px}
th,td{border-bottom:1px solid var(--line);padding:6px 10px;text-align:left;white-space:nowrap}thead th{color:var(--muted);font-weight:600}
.muted{color:var(--muted)}li{margin:4px 0}code{font-size:13px}";

pub fn page(title: &str, subtitle: &str, sections: &[(String, String)]) -> String {
    let mut o = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{CSS}</style></head><body><main><h1>{}</h1><p class=\"sub\">{}</p>",
        esc(title),
        esc(title),
        esc(subtitle)
    );
    for (h, body) in sections {
        o.push_str(&format!("<h2>{}</h2>{}", esc(h), body));
    }
    o.push_str("</main></body></html>\n");
    o
}

pub fn list_html(items: &[String]) -> String {
    if items.is_empty() {
        return "<p class=\"muted\">none</p>".into();
    }
    format!("<ul>{}</ul>", items.iter().map(|i| format!("<li>{}</li>", esc(i))).collect::<String>())
}
