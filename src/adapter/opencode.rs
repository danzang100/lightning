//! OpenCode adapter (`<data>/opencode/opencode.db`), opened read-only.
//!
//! Layout as documented by third-party readers in 2026 (no local data was available): tables
//! `session` (`id`, `directory`, `version`, `parent_id`, ...), `message` (`id`, `session_id`,
//! `data` JSON) and `part` (`id`, `message_id`, `session_id`, `data` JSON). Message data carries
//! `role`, `modelID`, `tokens {input, output, reasoning, cache {read, write}}` and
//! `time {created, completed}`; tool parts carry `tool`, `callID` and `state {status, input}`.
//! The newer `session_v2` / `session_message` layout is detected and reported, not parsed.
//! The legacy `storage/` JSON tree is migrated into the database by OpenCode and is not read.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{Adapter, DiscoveredFile};
use crate::classify;
use crate::redact::{Hasher, last_component, normalize_path};
use crate::schema::*;

pub struct OpenCode;

pub const UNSUPPORTED_V2: &str = "opencode_schema_v2_unsupported";

impl Adapter for OpenCode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn detect(&self, root: &Path) -> bool {
        root.join("opencode.db").is_file()
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredFile>> {
        let db = if root.is_file() { root.to_path_buf() } else { root.join("opencode.db") };
        Ok(if db.is_file() {
            vec![DiscoveredFile { path: db, project: String::new(), session_id: String::new(), is_subagent: false, agent_id: None }]
        } else {
            Vec::new()
        })
    }

    fn parse(&self, file: &DiscoveredFile, hasher: &Hasher) -> Result<ParsedFile> {
        let conn = Connection::open_with_flags(&file.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("open {} read-only", file.path.display()))?;
        parse_db(&conn, hasher)
    }
}

fn tables(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v)
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let v = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v)
}

fn ms_ts(v: Option<&Value>) -> Option<String> {
    let ms = v?.as_i64()?;
    Some(Utc.timestamp_millis_opt(ms).single()?.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str().filter(|x| !x.is_empty())
}

struct SessionRow {
    id: String,
    directory: Option<String>,
    version: Option<String>,
    parent: Option<String>,
}

pub fn parse_db(conn: &Connection, hasher: &Hasher) -> Result<ParsedFile> {
    let mut out = ParsedFile::default();
    let t = tables(conn)?;
    let has = |n: &str| t.iter().any(|x| x == n);
    if !(has("session") && has("message") && has("part")) {
        if has("session_v2") || has("session_message") {
            *out.stats.unknown_types.entry(UNSUPPORTED_V2.to_string()).or_default() += 1;
        }
        return Ok(out);
    }

    let scols = columns(conn, "session")?;
    let pick = |name: &str| if scols.iter().any(|c| c == name) { name.to_string() } else { "NULL".to_string() };
    let sql = format!(
        "SELECT id, {}, {}, {} FROM session",
        pick("directory"),
        pick("version"),
        if scols.iter().any(|c| c == "parent_id") { "parent_id".to_string() } else { pick("parentID") }
    );
    let mut sessions: HashMap<String, SessionRow> = HashMap::new();
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        sessions.insert(id.clone(), SessionRow { id, directory: r.get(1)?, version: r.get(2)?, parent: r.get(3)? });
    }
    drop(rows);
    drop(st);

    // Parts grouped by message, in id order (OpenCode ids sort by creation time).
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    let mut st = conn.prepare("SELECT message_id, data FROM part ORDER BY id")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        out.stats.lines += 1;
        let mid: String = r.get(0)?;
        let data: String = r.get(1)?;
        match serde_json::from_str::<Value>(&data) {
            Ok(v) => parts.entry(mid).or_default().push(v),
            Err(_) => out.stats.bad_lines += 1,
        }
    }
    drop(rows);
    drop(st);

    let mut st = conn.prepare("SELECT id, session_id, data FROM message ORDER BY session_id, id")?;
    let mut rows = st.query([])?;
    let mut prompts_seen: HashMap<String, usize> = HashMap::new();
    let mut span: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    while let Some(r) = rows.next()? {
        out.stats.lines += 1;
        let mid: String = r.get(0)?;
        let sid: String = r.get(1)?;
        let data: String = r.get(2)?;
        let Ok(m) = serde_json::from_str::<Value>(&data) else {
            out.stats.bad_lines += 1;
            continue;
        };
        let ts = ms_ts(m.get("time").and_then(|t| t.get("created")));
        if let Some(t) = &ts {
            let e = span.entry(sid.clone()).or_default();
            if e.0.as_ref().is_none_or(|f| t < f) {
                e.0 = Some(t.clone());
            }
            if e.1.as_ref().is_none_or(|l| t > l) {
                e.1 = Some(t.clone());
            }
        }
        let srow = sessions.get(&sid);
        let cwd = srow.and_then(|x| x.directory.clone());
        let is_sub = srow.is_some_and(|x| x.parent.is_some());
        let my_parts = parts.remove(&mid).unwrap_or_default();
        match s(&m, "role") {
            Some("user") => {
                let text: Vec<&str> = my_parts
                    .iter()
                    .filter(|p| s(p, "type") == Some("text") && p.get("synthetic").and_then(Value::as_bool) != Some(true))
                    .filter_map(|p| s(p, "text"))
                    .collect();
                if text.is_empty() {
                    continue;
                }
                let text = text.join("\n");
                let n = prompts_seen.entry(sid.clone()).or_default();
                let class = classify::prompt_class(&text, *n == 0);
                *n += 1;
                out.signals.push(UserSignal {
                    session_id: sid.clone(),
                    agent_id: String::new(),
                    ts,
                    kind: SignalKind::Prompt,
                    class: Some(class.as_str().to_string()),
                    task_hint: classify::task_hint(&text).map(String::from),
                    text_hash: Some(hasher.hash(text.trim())),
                    src_ref: Some(format!("message:{mid}")),
                });
            }
            Some("assistant") => {
                let tok = m.get("tokens");
                let num = |path: &[&str]| -> Option<i64> {
                    let mut v = tok?;
                    for k in path {
                        v = v.get(k)?;
                    }
                    v.as_i64()
                };
                let turn_id = hasher.hash(&format!("opencode|{mid}"));
                let project_name = cwd.as_deref().map(last_component).unwrap_or_else(|| "(unknown)".into());
                out.turns.push(Turn {
                    turn_id: turn_id.clone(),
                    session_id: sid.clone(),
                    agent_id: String::new(),
                    is_subagent: is_sub,
                    parent_session_id: srow.and_then(|x| x.parent.clone()),
                    project: cwd.as_deref().map(|c| hasher.hash(&normalize_path(c, None))).unwrap_or_default(),
                    project_name,
                    source_version: srow.and_then(|x| x.version.clone()),
                    model: s(&m, "modelID").unwrap_or("").to_string(),
                    ts: ts.clone(),
                    input_tokens: num(&["input"]),
                    output_tokens: num(&["output"]),
                    cache_read_tokens: num(&["cache", "read"]),
                    cache_write_5m_tokens: num(&["cache", "write"]),
                    cache_write_1h_tokens: None,
                    stop_reason: s(&m, "finish").map(String::from),
                    is_api_error: m.get("error").is_some_and(|e| !e.is_null()),
                    is_aborted: m.get("error").and_then(|e| s(e, "name")) == Some("MessageAbortedError"),
                    source: SOURCE_OPENCODE.to_string(),
                    effort: s(&m, "variant").map(String::from),
                    reasoning_tokens: num(&["reasoning"]),
                    agent_type: if is_sub { s(&m, "agent").or_else(|| s(&m, "mode")).map(String::from) } else { None },
                });
                for p in my_parts.iter().filter(|p| s(p, "type") == Some("tool")) {
                    let name = s(p, "tool").unwrap_or("unknown").to_string();
                    let kind = classify::tool_kind(&name);
                    let state = p.get("state").cloned().unwrap_or(Value::Null);
                    let input = state.get("input").cloned().unwrap_or(Value::Null);
                    let path = matches!(kind, "read" | "edit").then(|| s(&input, "filePath").or_else(|| s(&input, "path"))).flatten();
                    let call_id = s(p, "callID").or_else(|| s(p, "id")).unwrap_or("").to_string();
                    out.tool_calls.push(ToolCall {
                        tool_use_id: hasher.hash(&format!("opencode|{mid}|{call_id}")),
                        turn_id: turn_id.clone(),
                        session_id: sid.clone(),
                        agent_id: String::new(),
                        seq: out.tool_calls.len() as i64,
                        ts: ms_ts(state.get("time").and_then(|t| t.get("start"))).or_else(|| ts.clone()),
                        tool_name: name,
                        args_hash: hasher.hash_json(&input),
                        target_hash: path.map(|x| hasher.hash_path(x, cwd.as_deref())),
                        result_status: match s(&state, "status") {
                            Some("completed") => tool_exit(&state).map_or(ResultStatus::Ok, |ok| if ok { ResultStatus::Ok } else { ResultStatus::Error }),
                            Some("error") => ResultStatus::Error,
                            _ => ResultStatus::Unknown,
                        },
                        permission: if s(&state, "error").is_some_and(|e| e.contains("rejected permission")) { Permission::Denied } else { Permission::None },
                        denial_kind: None,
                        tool_kind: kind.to_string(),
                        target_kind: path.map(|x| classify::target_kind(&normalize_path(x, cwd.as_deref())).to_string()),
                        cmd_class: (kind == "shell").then(|| s(&input, "command").map(classify::cmd_class).unwrap_or("other").to_string()),
                    });
                }
            }
            other => {
                *out.stats.unknown_types.entry(format!("role:{}", other.unwrap_or("(none)"))).or_default() += 1;
            }
        }
    }

    let mut ids: Vec<&String> = sessions.keys().collect();
    ids.sort();
    for id in ids {
        let x = &sessions[id];
        let (start, end) = span.remove(&x.id).unwrap_or_default();
        out.sessions.push(Session {
            session_id: x.id.clone(),
            agent_id: String::new(),
            source: SOURCE_OPENCODE.to_string(),
            source_version: x.version.clone(),
            project: x.directory.as_deref().map(|c| hasher.hash(&normalize_path(c, None))).unwrap_or_default(),
            project_name: x.directory.as_deref().map(last_component).unwrap_or_else(|| "(unknown)".into()),
            cwd_hash: x.directory.as_deref().map(|c| hasher.hash(&normalize_path(c, None))),
            git_branch: None,
            started_at: start,
            ended_at: end,
            agent_type: None,
        });
    }
    Ok(out)
}

/// Bash parts record the exit code in `state.metadata.exit`.
fn tool_exit(state: &Value) -> Option<bool> {
    let m = state.get("metadata")?;
    m.get("exit").or_else(|| m.get("exitCode")).and_then(Value::as_i64).map(|c| c == 0)
}

/// Default OpenCode data directory (XDG data dir on every platform, as OpenCode uses).
pub fn default_root() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::paths::home().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("opencode"))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    /// Build a small OpenCode-shaped database.
    pub fn build_fixture(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, version TEXT, parent_id TEXT, time_created INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
        )
        .unwrap();
        c.execute("INSERT INTO session VALUES ('ses_a', '/home/u/proj', 'CANARY_OC_TITLE', '1.2.3', NULL, 0)", []).unwrap();
        c.execute("INSERT INTO session VALUES ('ses_b', '/home/u/proj', 't', '1.2.3', 'ses_a', 0)", []).unwrap();
        let msg = |id: &str, sid: &str, v: Value| {
            c.execute("INSERT INTO message VALUES (?1, ?2, 0, ?3)", rusqlite::params![id, sid, v.to_string()]).unwrap();
        };
        let part = |id: &str, mid: &str, v: Value| {
            c.execute("INSERT INTO part VALUES (?1, ?2, 'x', ?3)", rusqlite::params![id, mid, v.to_string()]).unwrap();
        };
        msg("msg_01", "ses_a", json!({"role": "user", "time": {"created": 1_780_000_000_000i64}}));
        part("prt_01", "msg_01", json!({"type": "text", "text": "Refactor the store module CANARY_OC_PROMPT"}));
        part("prt_02", "msg_01", json!({"type": "text", "text": "injected", "synthetic": true}));
        msg(
            "msg_02",
            "ses_a",
            json!({"role": "assistant", "modelID": "claude-sonnet-5", "providerID": "anthropic", "finish": "tool-calls",
                   "time": {"created": 1_780_000_005_000i64, "completed": 1_780_000_009_000i64},
                   "tokens": {"input": 100, "output": 20, "reasoning": 5, "cache": {"read": 1000, "write": 50}}}),
        );
        part("prt_03", "msg_02", json!({"type": "tool", "tool": "bash", "callID": "call_1",
            "state": {"status": "completed", "input": {"command": "cargo test CANARY_OC_CMD"}, "output": "CANARY_OC_OUT", "metadata": {"exit": 1}}}));
        part("prt_04", "msg_02", json!({"type": "tool", "tool": "edit", "callID": "call_2",
            "state": {"status": "completed", "input": {"filePath": "/home/u/proj/src/store.rs"}, "output": "ok"}}));
        part("prt_05", "msg_02", json!({"type": "tool", "tool": "read", "callID": "call_3",
            "state": {"status": "error", "input": {"filePath": "/home/u/proj/README.md"}, "error": "nope"}}));
        msg("msg_03", "ses_b", json!({"role": "assistant", "modelID": "claude-haiku-5", "agent": "explore", "time": {"created": 1_780_000_006_000i64},
                   "tokens": {"input": 10, "output": 2, "cache": {"read": 0, "write": 0}}}));
        c.execute("INSERT INTO part VALUES ('prt_06', 'msg_03', 'x', '{broken')", []).unwrap();
    }

    #[test]
    fn parses_sessions_turns_tools_and_prompts() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(&dir.path().join("opencode.db"));
        let files = OpenCode.discover(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        let p = OpenCode.parse(&files[0], &Hasher::new("t")).unwrap();
        assert_eq!(p.sessions.len(), 2);
        assert_eq!(p.turns.len(), 2);
        let t = p.turns.iter().find(|t| t.model == "claude-sonnet-5").unwrap();
        assert_eq!((t.input_tokens, t.output_tokens, t.cache_read_tokens, t.cache_write_5m_tokens, t.reasoning_tokens), (Some(100), Some(20), Some(1000), Some(50), Some(5)));
        assert_eq!((t.project_name.as_str(), t.source.as_str()), ("proj", SOURCE_OPENCODE));
        let sub = p.turns.iter().find(|t| t.is_subagent).unwrap();
        assert_eq!((sub.parent_session_id.as_deref(), sub.agent_type.as_deref()), (Some("ses_a"), Some("explore")));
        assert_eq!(p.tool_calls.len(), 3);
        assert_eq!((p.tool_calls[0].cmd_class.as_deref(), p.tool_calls[0].result_status), (Some("test"), ResultStatus::Error));
        assert_eq!(p.tool_calls[1].target_kind.as_deref(), Some("code"));
        assert_eq!((p.tool_calls[2].tool_kind.as_str(), p.tool_calls[2].result_status), ("read", ResultStatus::Error));
        assert_eq!(p.signals.len(), 1);
        assert_eq!((p.signals[0].class.as_deref(), p.signals[0].task_hint.as_deref()), (Some("new_task"), Some("refactor")));
        assert_eq!(p.stats.bad_lines, 1);
    }

    #[test]
    fn v2_layout_is_reported_not_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE session_v2 (id TEXT); CREATE TABLE session_message (id TEXT);").unwrap();
        let files = OpenCode.discover(dir.path()).unwrap();
        let p = OpenCode.parse(&files[0], &Hasher::new("t")).unwrap();
        assert!(p.turns.is_empty());
        assert_eq!(p.stats.unknown_types.get(UNSUPPORTED_V2), Some(&1));
    }
}
