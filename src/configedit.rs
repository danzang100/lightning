//! Minimal, format-preserving edits to harness config files, a unified diff, and the write
//! allowlist (lightning-spec 10.3, 13 "Write boundary"). Every edit is validated by re-parsing.
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Edit {
    /// Set a top-level string key in a JSON object.
    JsonKey { key: String, value: String },
    /// Set a top-level string key in a TOML document.
    TomlKey { key: String, value: String },
    /// Set a key in a Markdown file's YAML frontmatter.
    Frontmatter { key: String, value: String },
    /// Insert or replace a block between `<!-- lightning:{marker} -->` comments.
    MarkdownSection { marker: String, body: String },
}

impl Edit {
    pub fn apply(&self, current: Option<&str>) -> Result<String> {
        match self {
            Self::JsonKey { key, value } => json_set(current.unwrap_or(""), key, value),
            Self::TomlKey { key, value } => toml_set(current.unwrap_or(""), key, value),
            Self::Frontmatter { key, value } => frontmatter_set(current.context("agent definition file does not exist")?, key, value),
            Self::MarkdownSection { marker, body } => Ok(markdown_section(current.unwrap_or(""), marker, body)),
        }
    }
}

fn json_quote(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Byte span of a top-level key's value in a JSON object, if present.
fn json_top_value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let (mut depth, mut i) = (0i32, 0usize);
    let mut expect_key = false;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let end = i;
                if depth == 1 && expect_key {
                    let name: String = serde_json::from_str(&text[start..=end.min(b.len() - 1)]).unwrap_or_default();
                    // Skip to the colon, then the value.
                    let mut j = end + 1;
                    while j < b.len() && b[j] != b':' {
                        j += 1;
                    }
                    j += 1;
                    while j < b.len() && (b[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if name == key {
                        let vstart = j;
                        let (mut d, mut k, mut in_str) = (0i32, j, false);
                        while k < b.len() {
                            let c = b[k];
                            if in_str {
                                if c == b'\\' {
                                    k += 1;
                                } else if c == b'"' {
                                    in_str = false;
                                    if d == 0 {
                                        return Some((vstart, k + 1));
                                    }
                                }
                            } else {
                                match c {
                                    b'"' => in_str = true,
                                    b'{' | b'[' => d += 1,
                                    b'}' | b']' if d > 0 => {
                                        d -= 1;
                                        if d == 0 {
                                            return Some((vstart, k + 1));
                                        }
                                    }
                                    b',' | b'}' | b'\n' if d == 0 => {
                                        let mut e = k;
                                        while e > vstart && (b[e - 1] as char).is_whitespace() {
                                            e -= 1;
                                        }
                                        return Some((vstart, e));
                                    }
                                    _ => {}
                                }
                            }
                            k += 1;
                        }
                        return None;
                    }
                    expect_key = false;
                    i = j;
                    continue;
                }
            }
            b'{' | b'[' => {
                depth += 1;
                expect_key = b[i] == b'{' && depth == 1;
            }
            b'}' | b']' => depth -= 1,
            b',' if depth == 1 => expect_key = true,
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn json_set(text: &str, key: &str, value: &str) -> Result<String> {
    let out = if text.trim().is_empty() {
        format!("{{\n  {}: {}\n}}\n", json_quote(key), json_quote(value))
    } else {
        serde_json::from_str::<serde_json::Value>(text)
            .context("existing file is not valid JSON")?
            .as_object()
            .context("existing JSON is not an object")?;
        if let Some((s, e)) = json_top_value_span(text, key) {
            format!("{}{}{}", &text[..s], json_quote(value), &text[e..])
        } else {
            let open = text.find('{').context("no opening brace")?;
            let empty = text[open + 1..].trim_start().starts_with('}');
            let indent = text[open + 1..]
                .lines()
                .nth(1)
                .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "  ".into());
            let entry = format!("\n{indent}{}: {}{}", json_quote(key), json_quote(value), if empty { "" } else { "," });
            format!("{}{}{}", &text[..=open], entry, &text[open + 1..])
        }
    };
    let v: serde_json::Value = serde_json::from_str(&out).context("edit produced invalid JSON")?;
    if v.get(key).and_then(|x| x.as_str()) != Some(value) {
        bail!("edit did not set {key}");
    }
    Ok(out)
}

pub fn toml_set(text: &str, key: &str, value: &str) -> Result<String> {
    if !text.trim().is_empty() {
        toml::from_str::<toml::Table>(text).context("existing file is not valid TOML")?;
    }
    let line = format!("{key} = {}", json_quote(value));
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut done = false;
    for l in lines.iter_mut() {
        let t = l.trim_start();
        if t.starts_with('[') {
            break;
        }
        if let Some(rest) = t.strip_prefix(key)
            && rest.trim_start().starts_with('=')
        {
            *l = line.clone();
            done = true;
            break;
        }
    }
    if !done {
        let at = lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len());
        // Before the first table, after any leading top-level keys.
        lines.insert(at, line);
        if at < lines.len() - 1 && !lines[at + 1].trim().is_empty() {
            lines.insert(at + 1, String::new());
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    let t: toml::Table = toml::from_str(&out).context("edit produced invalid TOML")?;
    if t.get(key).and_then(|v| v.as_str()) != Some(value) {
        bail!("edit did not set {key}");
    }
    Ok(out)
}

pub fn frontmatter_set(text: &str, key: &str, value: &str) -> Result<String> {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split(nl).collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        bail!("file has no frontmatter");
    }
    let close = lines.iter().skip(1).position(|l| l.trim() == "---").map(|p| p + 1).context("unterminated frontmatter")?;
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let new = format!("{key}: {value}");
    match (1..close).find(|i| out[*i].split_once(':').is_some_and(|(k, _)| k.trim() == key)) {
        Some(i) => out[i] = new,
        None => out.insert(close, new),
    }
    Ok(out.join(nl))
}

pub fn markdown_section(text: &str, marker: &str, body: &str) -> String {
    let open = format!("<!-- lightning:{marker} -->");
    let close = format!("<!-- /lightning:{marker} -->");
    let block = format!("{open}\n{}\n{close}", body.trim_end());
    if let (Some(s), Some(e)) = (text.find(&open), text.find(&close)) {
        return format!("{}{}{}", &text[..s], block, &text[e + close.len()..]);
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&block);
    out.push('\n');
    out
}

/// Unified diff of two texts (line based, 3 lines of context).
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    // Edit script: (' ' | '-' | '+', a_index, b_index, line)
    let mut ops: Vec<(char, usize, usize, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', i, j, a[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] > lcs[i + 1][j]) {
            ops.push(('+', i, j, b[j]));
            j += 1;
        } else {
            ops.push(('-', i, j, a[i]));
            i += 1;
        }
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| o.0 != ' ').map(|(k, _)| k).collect();
    if changed.is_empty() {
        return out;
    }
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(3);
        let mut end = (changed[k] + 3).min(ops.len() - 1);
        while k + 1 < changed.len() && changed[k + 1] <= end + 4 {
            k += 1;
            end = (changed[k] + 3).min(ops.len() - 1);
        }
        let hunk = &ops[start..=end];
        let a_start = hunk.iter().find(|o| o.0 != '+').map_or(hunk[0].1, |o| o.1);
        let b_start = hunk.iter().find(|o| o.0 != '-').map_or(hunk[0].2, |o| o.2);
        let a_len = hunk.iter().filter(|o| o.0 != '+').count();
        let b_len = hunk.iter().filter(|o| o.0 != '-').count();
        out.push_str(&format!("@@ -{},{} +{},{} @@\n", a_start + (a_len > 0) as usize, a_len, b_start + (b_len > 0) as usize, b_len));
        for o in hunk {
            out.push(o.0);
            out.push_str(o.3);
            out.push('\n');
        }
        k += 1;
    }
    out
}

/// Config paths `apply` may write, relative to the home directory or the repo root.
pub const USER_ALLOWLIST: [&str; 7] = [
    ".claude/settings.json",
    ".claude/CLAUDE.md",
    ".claude/agents/*.md",
    ".codex/config.toml",
    ".codex/AGENTS.md",
    ".config/opencode/opencode.json",
    ".config/opencode/AGENTS.md",
];
pub const REPO_ALLOWLIST: [&str; 6] = [".claude/settings.json", ".claude/agents/*.md", "CLAUDE.md", "AGENTS.md", ".codex/config.toml", "opencode.json"];

fn matches(rel: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => rel == pattern,
        Some((pre, suf)) => rel.len() > pre.len() + suf.len() && rel.starts_with(pre) && rel.ends_with(suf) && !rel[pre.len()..rel.len() - suf.len()].contains('/'),
    }
}

/// True when `rel` is a plain relative path (no `..`, not absolute) on the allowlist.
pub fn allowed(rel: &str, user_level: bool) -> bool {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return false;
    }
    let rel = rel.replace('\\', "/");
    let list: &[&str] = if user_level { &USER_ALLOWLIST } else { &REPO_ALLOWLIST };
    list.iter().any(|pat| matches(&rel, pat))
}

/// Join and check a target; refuses anything off the allowlist.
pub fn resolve(base: &Path, rel: &str, user_level: bool) -> Result<PathBuf> {
    if !allowed(rel, user_level) {
        bail!("{rel} is not on the {} allowlist; refusing to write it", if user_level { "user" } else { "repo" });
    }
    Ok(base.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_replaces_only_the_top_level_key_and_keeps_formatting() {
        let src = "{\n  \"permissions\": {\"model\": \"keep\"},\n  \"model\": \"opus\",\n  \"x\": [1, 2]\n}\n";
        let out = json_set(src, "model", "sonnet").unwrap();
        assert_eq!(out, "{\n  \"permissions\": {\"model\": \"keep\"},\n  \"model\": \"sonnet\",\n  \"x\": [1, 2]\n}\n");
        let out = json_set("{\n  \"a\": 1\n}\n", "model", "m").unwrap();
        assert_eq!(out, "{\n  \"model\": \"m\",\n  \"a\": 1\n}\n");
        assert_eq!(json_set("", "model", "m").unwrap(), "{\n  \"model\": \"m\"\n}\n");
        assert_eq!(json_set("{}", "model", "m").unwrap(), "{\n  \"model\": \"m\"}");
        assert!(json_set("[1]", "model", "m").is_err());
        assert!(json_set("{bad", "model", "m").is_err());
        let out = json_set("{\"model\": null, \"b\": 2}", "model", "m").unwrap();
        assert_eq!(out, "{\"model\": \"m\", \"b\": 2}");
    }

    #[test]
    fn toml_sets_top_level_before_tables() {
        let src = "# comment\nmodel = \"gpt-a\"\n\n[mcp_servers.x]\nmodel = \"inner\"\n";
        assert_eq!(toml_set(src, "model", "gpt-b").unwrap(), "# comment\nmodel = \"gpt-b\"\n\n[mcp_servers.x]\nmodel = \"inner\"\n");
        let src = "approval_policy = \"on-request\"\n[profiles.x]\nmodel = \"inner\"\n";
        assert_eq!(toml_set(src, "model", "m").unwrap(), "approval_policy = \"on-request\"\nmodel = \"m\"\n\n[profiles.x]\nmodel = \"inner\"\n");
        assert_eq!(toml_set("", "model", "m").unwrap(), "model = \"m\"\n");
    }

    #[test]
    fn frontmatter_and_sections() {
        let src = "---\nname: rev\nmodel: opus\n---\nBody\n";
        assert_eq!(frontmatter_set(src, "model", "sonnet").unwrap(), "---\nname: rev\nmodel: sonnet\n---\nBody\n");
        assert_eq!(frontmatter_set("---\nname: rev\n---\nB", "model", "x").unwrap(), "---\nname: rev\nmodel: x\n---\nB");
        assert!(frontmatter_set("no fm", "model", "x").is_err());
        let once = markdown_section("# Notes\n", "rule-1", "Do X.");
        assert_eq!(once, "# Notes\n\n<!-- lightning:rule-1 -->\nDo X.\n<!-- /lightning:rule-1 -->\n");
        assert_eq!(markdown_section(&once, "rule-1", "Do Y."), once.replace("Do X.", "Do Y."));
    }

    #[test]
    fn diff_shows_changes_with_context() {
        let d = unified_diff("f.json", "a\nb\nc\nd\n", "a\nB\nc\nd\n");
        assert_eq!(d, "--- a/f.json\n+++ b/f.json\n@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n");
        let d = unified_diff("f", "", "x\n");
        assert_eq!(d, "--- a/f\n+++ b/f\n@@ -0,0 +1,1 @@\n+x\n");
    }

    #[test]
    fn allowlist_blocks_everything_else() {
        assert!(allowed(".claude/settings.json", true));
        assert!(allowed(".claude/agents/reviewer.md", false));
        assert!(!allowed(".claude/agents/sub/reviewer.md", false));
        assert!(!allowed("../.claude/settings.json", false));
        assert!(!allowed("/etc/passwd", true));
        assert!(!allowed(".bashrc", true));
        assert!(!allowed("CLAUDE.md", true));
        assert!(allowed("CLAUDE.md", false));
    }
}
