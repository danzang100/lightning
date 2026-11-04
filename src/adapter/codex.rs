//! Codex rollout adapter (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`).
//!
//! Shapes verified against local rollouts from Codex 0.123 (2026-11-04): `session_meta`,
//! `turn_context`, `event_msg` (`user_message`, `token_count`, `exec_command_end`,
//! `patch_apply_end`, `turn_aborted`, `thread_rolled_back`, `error`) and `response_item`
//! (`function_call`, `custom_tool_call` and their outputs). Files are only ever read.
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::{Adapter, DiscoveredFile};
use crate::classify;
use crate::redact::{Hasher, last_component, normalize_path};
use crate::schema::*;

pub struct Codex;

const CONSUMED: [&str; 13] = [
    "session_meta",
    "turn_context",
    "user_message",
    "token_count",
    "function_call",
    "custom_tool_call",
    "function_call_output",
    "custom_tool_call_output",
    "exec_command_end",
    "patch_apply_end",
    "turn_aborted",
    "thread_rolled_back",
    "error",
];

impl Adapter for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn detect(&self, root: &Path) -> bool {
        root.is_dir()
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredFile>> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = std::fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))?;
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_rollout(&p) {
                    out.push(DiscoveredFile {
                        session_id: session_from_name(&p),
                        path: p,
                        project: String::new(),
                        is_subagent: false,
                        agent_id: None,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn parse(&self, file: &DiscoveredFile, hasher: &Hasher) -> Result<ParsedFile> {
        let f = File::open(&file.path).with_context(|| format!("open {}", file.path.display()))?;
        parse_reader(BufReader::new(f), file, hasher)
    }
}

fn is_rollout(p: &Path) -> bool {
    let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// `rollout-2026-07-16T14-57-45-<uuid>.jsonl` -> `<uuid>`.
fn session_from_name(p: &Path) -> String {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 { parts[parts.len() - 5..].join("-") } else { stem }
}

fn norm_ts(v: Option<&Value>) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(v?.as_str()?).ok()?.with_timezone(&Utc);
    Some(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str().filter(|x| !x.is_empty())
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key)?.as_i64()
}

/// Exit status from a tool output: "Exit code: N" text, or JSON with `metadata.exit_code`.
fn output_ok(output: &Value) -> Option<bool> {
    let text = match output {
        Value::String(t) => t.clone(),
        Value::Object(_) => output.to_string(),
        _ => return None,
    };
    if let Ok(v) = serde_json::from_str::<Value>(&text)
        && let Some(code) = v.get("metadata").and_then(|m| i(m, "exit_code"))
    {
        return Some(code == 0);
    }
    if let Some(rest) = text.trim_start().strip_prefix("Exit code:") {
        let code: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
        return code.parse::<i64>().ok().map(|c| c == 0);
    }
    if text.starts_with("execution error") || text.starts_with("error") {
        return Some(false);
    }
    None
}

fn shell_command(args: &Value) -> Option<String> {
    match args.get("command")? {
        Value::String(c) => Some(c.clone()),
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// First file named in an apply_patch body.
fn patch_target(patch: &str) -> Option<String> {
    patch.lines().find_map(|l| {
        ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .map(|x| x.trim().to_string())
    })
}

pub fn parse_reader<R: BufRead>(mut reader: R, file: &DiscoveredFile, hasher: &Hasher) -> Result<ParsedFile> {
    let mut out = ParsedFile::default();
    let mut session_id = file.session_id.clone();
    let (mut cwd, mut version): (Option<String>, Option<String>) = (None, None);
    let (mut model, mut effort): (String, Option<String>) = (String::new(), None);
    let (mut first_ts, mut last_ts): (Option<String>, Option<String>) = (None, None);
    let mut last_total: Option<i64> = None;
    // Tool calls are written before the token_count of the API call that made them.
    let mut unassigned: Vec<usize> = Vec::new();
    let mut call_index: HashMap<String, usize> = HashMap::new();
    let mut pending_error = false;
    let mut prompts_seen = 0usize;
    let mut line_no = 0u64;
    let mut buf = Vec::new();

    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        line_no += 1;
        let complete = buf.ends_with(b"\n");
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.stats.lines += 1;
        let line: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) if !complete => {
                out.stats.partial_tail = true;
                continue;
            }
            Err(_) => {
                out.stats.bad_lines += 1;
                continue;
            }
        };
        let ts = norm_ts(line.get("timestamp"));
        if let Some(t) = &ts {
            if first_ts.as_ref().is_none_or(|f| t < f) {
                first_ts = Some(t.clone());
            }
            if last_ts.as_ref().is_none_or(|l| t > l) {
                last_ts = Some(t.clone());
            }
        }
        let outer = line.get("type").and_then(Value::as_str).unwrap_or("(none)");
        let empty = Value::Null;
        let p = line.get("payload").unwrap_or(&empty);
        let ty = match outer {
            "session_meta" | "turn_context" => outer,
            _ => p.get("type").and_then(Value::as_str).unwrap_or(outer),
        };
        if !CONSUMED.contains(&ty) {
            *out.stats.unknown_types.entry(ty.to_string()).or_default() += 1;
            continue;
        }
        match ty {
            "session_meta" => {
                if let Some(id) = s(p, "id").or_else(|| s(p, "session_id")) {
                    session_id = id.to_string();
                }
                cwd = cwd.or_else(|| s(p, "cwd").map(String::from));
                version = version.or_else(|| s(p, "cli_version").map(String::from));
            }
            "turn_context" => {
                if let Some(m) = s(p, "model") {
                    model = m.to_string();
                }
                effort = s(p, "effort")
                    .or_else(|| p.get("collaboration_mode").and_then(|c| c.get("settings")).and_then(|x| s(x, "reasoning_effort")))
                    .map(String::from)
                    .or(effort);
                cwd = cwd.or_else(|| s(p, "cwd").map(String::from));
            }
            "user_message" => {
                let msg = s(p, "message").unwrap_or("");
                let class = classify::prompt_class(msg, prompts_seen == 0);
                prompts_seen += 1;
                out.signals.push(UserSignal {
                    session_id: session_id.clone(),
                    agent_id: String::new(),
                    ts,
                    kind: SignalKind::Prompt,
                    class: Some(class.as_str().to_string()),
                    task_hint: classify::task_hint(msg).map(String::from),
                    text_hash: Some(hasher.hash(msg.trim())),
                    src_ref: Some(format!("line:{line_no}")),
                });
            }
            "turn_aborted" | "thread_rolled_back" => {
                let class = if ty == "turn_aborted" { "abort" } else { "correction" };
                if ty == "turn_aborted"
                    && let Some(t) = out.turns.last_mut()
                {
                    t.is_aborted = true;
                }
                out.signals.push(UserSignal {
                    session_id: session_id.clone(),
                    agent_id: String::new(),
                    ts,
                    kind: SignalKind::Prompt,
                    class: Some(class.to_string()),
                    task_hint: None,
                    text_hash: None,
                    src_ref: Some(format!("line:{line_no}")),
                });
            }
            "error" => pending_error = true,
            "token_count" => {
                let Some(info) = p.get("info").filter(|v| v.is_object()) else { continue };
                // The same totals are often written twice; a turn is a change in the running total.
                let total = info.get("total_token_usage").and_then(|u| i(u, "total_tokens"));
                if total.is_some() && total == last_total {
                    continue;
                }
                last_total = total;
                let Some(u) = info.get("last_token_usage") else { continue };
                let input = i(u, "input_tokens");
                let cached = i(u, "cached_input_tokens");
                let turn_id = hasher.hash(&format!("codex|{session_id}|{line_no}"));
                for idx in unassigned.drain(..) {
                    out.tool_calls[idx].turn_id = turn_id.clone();
                }
                out.turns.push(Turn {
                    turn_id,
                    session_id: session_id.clone(),
                    agent_id: String::new(),
                    is_subagent: false,
                    parent_session_id: None,
                    project: String::new(),
                    project_name: String::new(),
                    source_version: version.clone(),
                    model: model.clone(),
                    ts,
                    // Codex counts cached input inside input_tokens; store them apart like Claude.
                    input_tokens: input.map(|x| x - cached.unwrap_or(0)),
                    output_tokens: i(u, "output_tokens"),
                    cache_read_tokens: cached,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    stop_reason: None,
                    is_api_error: std::mem::take(&mut pending_error),
                    is_aborted: false,
                    source: SOURCE_CODEX.to_string(),
                    effort: effort.clone(),
                    reasoning_tokens: i(u, "reasoning_output_tokens"),
                    agent_type: None,
                });
            }
            "function_call" | "custom_tool_call" => {
                let Some(call_id) = s(p, "call_id") else { continue };
                if call_index.contains_key(call_id) {
                    continue;
                }
                let name = s(p, "name").unwrap_or("unknown").to_string();
                let kind = classify::tool_kind(&name);
                let raw = if ty == "function_call" { p.get("arguments") } else { p.get("input") };
                let raw_text = raw.and_then(Value::as_str).unwrap_or("");
                let args: Value = serde_json::from_str(raw_text).unwrap_or_else(|_| Value::String(raw_text.to_string()));
                let path = match kind {
                    "edit" => patch_target(raw_text),
                    "read" => s(&args, "path").or_else(|| s(&args, "file_path")).map(String::from),
                    _ => None,
                };
                let cwd_ref = cwd.as_deref();
                call_index.insert(call_id.to_string(), out.tool_calls.len());
                unassigned.push(out.tool_calls.len());
                out.tool_calls.push(ToolCall {
                    tool_use_id: hasher.hash(&format!("codex|{session_id}|{call_id}")),
                    turn_id: String::new(),
                    session_id: session_id.clone(),
                    agent_id: String::new(),
                    seq: out.tool_calls.len() as i64,
                    ts,
                    tool_name: name,
                    args_hash: hasher.hash_json(&args),
                    target_hash: path.as_deref().map(|x| hasher.hash_path(x, cwd_ref)),
                    result_status: ResultStatus::Unknown,
                    permission: Permission::None,
                    denial_kind: None,
                    tool_kind: kind.to_string(),
                    target_kind: path.as_deref().map(|x| classify::target_kind(&normalize_path(x, cwd_ref)).to_string()),
                    cmd_class: (kind == "shell").then(|| shell_command(&args).as_deref().map(classify::cmd_class).unwrap_or("other").to_string()),
                });
            }
            "function_call_output" | "custom_tool_call_output" => {
                let Some(&idx) = s(p, "call_id").and_then(|c| call_index.get(c)) else { continue };
                if out.tool_calls[idx].result_status == ResultStatus::Unknown
                    && let Some(ok) = p.get("output").and_then(output_ok)
                {
                    out.tool_calls[idx].result_status = if ok { ResultStatus::Ok } else { ResultStatus::Error };
                }
            }
            "exec_command_end" | "patch_apply_end" => {
                let Some(&idx) = s(p, "call_id").and_then(|c| call_index.get(c)) else { continue };
                let ok = if ty == "exec_command_end" {
                    i(p, "exit_code").map(|c| c == 0)
                } else {
                    p.get("success").and_then(Value::as_bool)
                };
                if let Some(ok) = ok {
                    out.tool_calls[idx].result_status = if ok { ResultStatus::Ok } else { ResultStatus::Error };
                }
            }
            _ => {}
        }
    }

    // Calls after the last token_count belong to the last turn.
    if let Some(last) = out.turns.last().map(|t| t.turn_id.clone()) {
        for idx in unassigned.drain(..) {
            out.tool_calls[idx].turn_id = last.clone();
        }
    } else {
        out.tool_calls.clear();
    }

    let project_name = cwd.as_deref().map(last_component).filter(|x| !x.is_empty()).unwrap_or_else(|| "(unknown)".into());
    let project = cwd.as_deref().map(|c| hasher.hash(&normalize_path(c, None))).unwrap_or_default();
    for t in &mut out.turns {
        t.session_id = session_id.clone();
        t.project = project.clone();
        t.project_name = project_name.clone();
    }
    for c in &mut out.tool_calls {
        c.session_id = session_id.clone();
    }
    for sig in &mut out.signals {
        sig.session_id = session_id.clone();
    }
    if out.stats.lines > 0 {
        out.sessions.push(Session {
            session_id,
            agent_id: String::new(),
            source: SOURCE_CODEX.to_string(),
            source_version: version,
            project,
            project_name,
            cwd_hash: cwd.as_deref().map(|c| hasher.hash(&normalize_path(c, None))),
            git_branch: None,
            started_at: first_ts,
            ended_at: last_ts,
            agent_type: None,
        });
    }
    Ok(out)
}

/// Default Codex sessions root.
pub fn default_root() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::paths::home().map(|h| h.join(".codex")))
        .map(|c| c.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_root")
    }

    fn parsed() -> ParsedFile {
        let files = Codex.discover(&fixtures()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].session_id, "019f0000-0000-7000-8000-000000000001");
        Codex.parse(&files[0], &Hasher::new("t")).unwrap()
    }

    #[test]
    fn turns_dedupe_repeated_token_counts_and_split_cached_input() {
        let p = parsed();
        assert_eq!(p.turns.len(), 3);
        let t = &p.turns[0];
        assert_eq!(t.model, "gpt-5.4-mini");
        assert_eq!(t.effort.as_deref(), Some("medium"));
        assert_eq!((t.input_tokens, t.cache_read_tokens, t.output_tokens, t.reasoning_tokens), (Some(600), Some(400), Some(50), Some(10)));
        assert_eq!(p.turns[2].effort.as_deref(), Some("high"));
        assert!(p.turns[2].is_aborted);
        assert!(p.turns.iter().all(|t| t.source == SOURCE_CODEX && t.project_name == "demo"));
    }

    #[test]
    fn tool_calls_get_kinds_classes_and_results() {
        let p = parsed();
        assert_eq!(p.tool_calls.len(), 3);
        let shell = &p.tool_calls[0];
        assert_eq!((shell.tool_kind.as_str(), shell.cmd_class.as_deref()), ("shell", Some("test")));
        assert_eq!(shell.result_status, ResultStatus::Error);
        assert_eq!(shell.turn_id, p.turns[0].turn_id);
        let patch = &p.tool_calls[1];
        assert_eq!((patch.tool_kind.as_str(), patch.target_kind.as_deref()), ("edit", Some("code")));
        assert_eq!(patch.result_status, ResultStatus::Ok);
        assert_eq!(patch.turn_id, p.turns[1].turn_id);
        assert_eq!(p.tool_calls[2].result_status, ResultStatus::Ok);
    }

    #[test]
    fn prompts_and_aborts_become_signals_and_garbage_is_counted() {
        let p = parsed();
        let classes: Vec<_> = p.signals.iter().map(|s| s.class.clone().unwrap()).collect();
        assert_eq!(classes, vec!["new_task", "correction", "abort"]);
        assert_eq!(p.signals[0].task_hint.as_deref(), Some("debugging"));
        assert_eq!(p.stats.bad_lines, 1);
        assert_eq!(p.stats.unknown_types.get("reasoning"), Some(&1));
        assert_eq!(p.sessions[0].source_version.as_deref(), Some("0.123.0"));
    }

    #[test]
    fn exit_codes_parse() {
        assert_eq!(output_ok(&Value::String("Exit code: 0\nWall time: 1s".into())), Some(true));
        assert_eq!(output_ok(&Value::String("Exit code: 101\n".into())), Some(false));
        assert_eq!(output_ok(&Value::String(r#"{"output":"x","metadata":{"exit_code":0}}"#.into())), Some(true));
        assert_eq!(output_ok(&Value::String("hello".into())), None);
    }

    #[test]
    fn never_panics_on_truncation() {
        let files = Codex.discover(&fixtures()).unwrap();
        let text = std::fs::read(&files[0].path).unwrap();
        for cut in 0..text.len() {
            parse_reader(&text[..cut], &files[0], &Hasher::new("t")).unwrap();
        }
    }
}
