//! JSONL export of normalized records (lightning-spec 8, N7). Field names follow the
//! OpenTelemetry GenAI semantic conventions where a stable-enough name exists, with a `lightning.`
//! namespace for the rest. The conventions are still at development status; see `OTEL_GENAI`.
use std::io::Write;

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::schema::SCHEMA_VERSION;
use crate::segment::Segment;
use crate::store::Store;

/// The GenAI semantic-conventions release the `gen_ai.*` names were taken from.
pub const OTEL_GENAI: &str = "1.37.0";

fn row_to_map(names: &[&str], row: &rusqlite::Row) -> rusqlite::Result<Map<String, Value>> {
    let mut m = Map::new();
    for (i, n) in names.iter().enumerate() {
        let v: rusqlite::types::Value = row.get(i)?;
        let j = match v {
            rusqlite::types::Value::Null => Value::Null,
            rusqlite::types::Value::Integer(x) => json!(x),
            rusqlite::types::Value::Real(x) => json!(x),
            rusqlite::types::Value::Text(x) => json!(x),
            rusqlite::types::Value::Blob(_) => Value::Null,
        };
        m.insert((*n).to_string(), j);
    }
    Ok(m)
}

fn emit<W: Write>(out: &mut W, record: &str, mut m: Map<String, Value>) -> Result<()> {
    m.insert("record".into(), json!(record));
    m.insert("schema_version".into(), json!(SCHEMA_VERSION));
    writeln!(out, "{}", Value::Object(m))?;
    Ok(())
}

pub fn jsonl<W: Write>(store: &Store, segs: &[Segment], out: &mut W) -> Result<u64> {
    let c = store.conn();
    let mut n = 0;
    writeln!(out, "{}", json!({"record": "header", "schema_version": SCHEMA_VERSION, "otel_genai_semconv": OTEL_GENAI, "generator": format!("lightning {}", env!("CARGO_PKG_VERSION"))}))?;

    let queries: [(&str, &str, &[&str]); 4] = [
        (
            "session",
            "SELECT session_id, agent_id, source, source_version, project_name, cwd_hash, git_branch, started_at, ended_at, agent_type FROM sessions ORDER BY started_at",
            &["gen_ai.conversation.id", "lightning.agent_id", "lightning.harness", "lightning.harness_version", "lightning.repo", "lightning.repo_hash", "lightning.git_branch", "lightning.start", "lightning.end", "gen_ai.agent.name"],
        ),
        (
            "turn",
            "SELECT turn_id, session_id, agent_id, is_subagent, COALESCE(source, 'claude_code'), model, effort, ts, input_tokens, output_tokens, cache_read_tokens,
                    COALESCE(cache_write_5m_tokens, 0) + COALESCE(cache_write_1h_tokens, 0), reasoning_tokens, stop_reason, is_api_error, is_aborted, agent_type
             FROM turns ORDER BY ts",
            &[
                "lightning.turn_id",
                "gen_ai.conversation.id",
                "lightning.agent_id",
                "lightning.is_subagent",
                "lightning.harness",
                "gen_ai.request.model",
                "lightning.effort",
                "lightning.ts",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "lightning.usage.cache_read_tokens",
                "lightning.usage.cache_write_tokens",
                "lightning.usage.reasoning_tokens",
                "gen_ai.response.finish_reasons",
                "lightning.is_api_error",
                "lightning.is_aborted",
                "gen_ai.agent.name",
            ],
        ),
        (
            "tool_call",
            "SELECT tool_use_id, turn_id, session_id, ts, tool_name, tool_kind, args_hash, target_hash, target_kind, cmd_class, result_status, permission FROM tool_calls ORDER BY ts",
            &[
                "gen_ai.tool.call.id",
                "lightning.turn_id",
                "gen_ai.conversation.id",
                "lightning.ts",
                "gen_ai.tool.name",
                "lightning.tool_kind",
                "lightning.args_hash",
                "lightning.target_hash",
                "lightning.target_kind",
                "lightning.cmd_class",
                "lightning.status",
                "lightning.permission",
            ],
        ),
        (
            "user_signal",
            "SELECT session_id, ts, class, task_hint, text_hash FROM signals WHERE kind = 'prompt' AND class IS NOT NULL ORDER BY ts",
            &["gen_ai.conversation.id", "lightning.ts", "lightning.class", "lightning.task_hint", "lightning.text_hash"],
        ),
    ];
    for (record, sql, names) in queries {
        let mut st = c.prepare(sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut m = row_to_map(names, r)?;
            if record == "turn"
                && let Some(Value::String(f)) = m.get("gen_ai.response.finish_reasons").cloned()
            {
                m.insert("gen_ai.response.finish_reasons".into(), json!([f]));
            }
            emit(out, record, m)?;
            n += 1;
        }
    }
    for s in segs {
        let mut m = match serde_json::to_value(s)? {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        // Source pointers are local paths' row ids; leave them out of an export.
        m.remove("prompt_file_id");
        m.remove("prompt_ref");
        emit(out, "segment", m)?;
        n += 1;
    }
    Ok(n)
}
