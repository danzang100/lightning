//! Every CLI surface end to end on synthetic and fixture data.
use std::path::{Path, PathBuf};

use clap::Parser;
use lightning::cli::{Cli, run_with_input};
use lightning::synth::{self, PairSpec, Plan};

fn fixtures(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn cli_in(db: &Path, pricing: &Path, args: &[&str], input: &str) -> String {
    let mut full = vec!["lightning".to_string(), "--db".into(), db.to_string_lossy().into_owned(), "--pricing".into(), pricing.to_string_lossy().into_owned()];
    full.extend(args.iter().map(|s| s.to_string()));
    let parsed = Cli::try_parse_from(full).unwrap();
    let mut out = Vec::new();
    run_with_input(parsed, &mut input.as_bytes(), &mut out).unwrap_or_else(|e| panic!("{args:?}: {e:#}"));
    String::from_utf8(out).unwrap()
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    pricing: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("claude");
    let plan = Plan { pairs: vec![PairSpec::new("big-model", 0.9, 1.0), PairSpec::new("small-model", 0.9, 0.3)], tasks: vec!["debugging", "refactor"], segments: 30, ..Plan::default() };
    synth::generate(&root, &plan).unwrap();
    let pricing = tmp.path().join("pricing.toml");
    let mut t = String::from("version = \"2026-11-01\"\n");
    for m in ["big-model", "small-model"] {
        t.push_str(&format!("[[model]]\nid = \"{m}\"\ninput_per_mtok = 3.0\noutput_per_mtok = 15.0\ncache_read_per_mtok = 0.3\ncache_write_5m_per_mtok = 3.75\ncache_write_1h_per_mtok = 6.0\n"));
    }
    std::fs::write(&pricing, t).unwrap();
    let db = tmp.path().join("db/l.db");
    World { root, db, pricing, _tmp: tmp }
}

#[test]
fn ingest_two_harnesses_and_report_by_pair_and_task() {
    let w = world();
    let out = cli_in(&w.db, &w.pricing, &["ingest", "--root", &w.root.to_string_lossy()], "");
    assert!(out.contains("sources: claude"), "{out}");
    let out = cli_in(&w.db, &w.pricing, &["ingest", "--source", "codex", "--root", &fixtures("codex_root").to_string_lossy(), "--json"], "");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["sources"][0], "codex");

    let table = cli_in(&w.db, &w.pricing, &["report"], "");
    assert!(table.contains("claude_code/big-model") && table.contains("debugging") && table.contains("refactor"), "{table}");
    assert!(table.contains("codex/gpt-5.4-mini"), "{table}");
    let json = cli_in(&w.db, &w.pricing, &["report", "--by", "pair", "--format", "json"], "");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["cells"].as_array().unwrap().iter().any(|c| c["pair"] == "claude_code/small-model" && c["cost_basis"] == "usd"));
    let html = cli_in(&w.db, &w.pricing, &["report", "--format", "html"], "");
    assert!(html.starts_with("<!doctype html>") && html.contains("<table>"));
    let effort = cli_in(&w.db, &w.pricing, &["report", "--by", "pair+effort", "--format", "md"], "");
    assert!(effort.contains("claude_code/big-model@medium"), "{effort}");

    let cmp = cli_in(&w.db, &w.pricing, &["compare", "big-model", "small-model", "--task", "debugging"], "");
    assert!(cmp.contains("verdict:") && cmp.contains("big-model: n=") && cmp.contains("success difference (B - A, 90%)"), "{cmp}");
    let pick = cli_in(&w.db, &w.pricing, &["pick", "fix the crash on startup", "--since", "3650d"], "");
    assert!(pick.contains("task type: debugging") && pick.contains("1. claude_code/") && pick.contains("note: proxy success is a heuristic"), "{pick}");
    let pick: serde_json::Value = serde_json::from_str(&cli_in(&w.db, &w.pricing, &["pick", "fix the crash on startup", "--since", "3650d", "--repo", "no-such-repo", "--json"], "")).unwrap();
    assert_eq!(pick["verdict"], "insufficient_data");
    let segs = cli_in(&w.db, &w.pricing, &["segments", "--limit", "5"], "");
    assert_eq!(segs.lines().count(), 5);
    let sw = cli_in(&w.db, &w.pricing, &["switches", "--since", "3650d"], "");
    assert!(sw.contains("Lightning switches"));
    let doc = cli_in(&w.db, &w.pricing, &["doctor"], "");
    assert!(doc.contains("claude_code=") && doc.contains("codex="), "{doc}");
}

#[test]
fn label_suggest_export_weekly_and_mcp() {
    let w = world();
    cli_in(&w.db, &w.pricing, &["ingest", "--root", &w.root.to_string_lossy()], "");
    let segs: serde_json::Value = serde_json::from_str(&cli_in(&w.db, &w.pricing, &["segments", "--json", "--limit", "3"], "")).unwrap();
    let id = segs[0]["segment_id"].as_str().unwrap().to_string();

    let shown = cli_in(&w.db, &w.pricing, &["label", "show", &id[..10]], "");
    assert!(shown.contains("prompt: Fix the crash") || shown.contains("prompt: Refactor"), "{shown}");
    cli_in(&w.db, &w.pricing, &["label", "set", &id, "--task", "debugging", "--outcome", "success"], "");
    let labeled = cli_in(&w.db, &w.pricing, &["label", "next", "--sample", "2"], "1\ns\n\n\n");
    assert!(labeled.contains("labeled 2"), "{labeled}");
    let gate = cli_in(&w.db, &w.pricing, &["label", "agreement"], "");
    assert!(gate.contains("insufficient_labels"), "{gate}");

    let sug = cli_in(&w.db, &w.pricing, &["--min-n", "10", "suggest", "--json"], "");
    let v: serde_json::Value = serde_json::from_str(&sug).unwrap();
    assert!(v.as_array().unwrap().iter().all(|s| s["status"] == "pending"));
    let listed = cli_in(&w.db, &w.pricing, &["suggest", "--list"], "");
    assert!(!listed.is_empty());

    let export = cli_in(&w.db, &w.pricing, &["export"], "");
    let first: serde_json::Value = serde_json::from_str(export.lines().next().unwrap()).unwrap();
    assert_eq!(first["record"], "header");
    assert!(export.lines().any(|l| l.contains("\"gen_ai.request.model\":\"big-model\"")));
    assert!(export.lines().any(|l| l.contains("\"record\":\"segment\"")));

    let dir = w.db.parent().unwrap().join("reports");
    let out = cli_in(&w.db, &w.pricing, &["weekly", "--no-ingest", "--out", &dir.to_string_lossy()], "");
    assert!(out.contains("weekly-"), "{out}");
    let html = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).find(|e| e.path().extension().is_some_and(|x| x == "html")).unwrap();
    let html = std::fs::read_to_string(html.path()).unwrap();
    assert!(html.contains("Lightning: week of") && html.contains("prefers-color-scheme"));

    let req = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recommend_pair","arguments":{"task_description":"fix the failing build"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        r#"not json"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
    ]
    .join("\n");
    let out = cli_in(&w.db, &w.pricing, &["mcp"], &req);
    let replies: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 6, "a notification gets no reply");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "lightning");
    let names: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["pair_report", "recommend_pair", "list_suggestions", "get_suggestion", "adoption_review"]);
    let text = replies[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"task_type\": \"debugging\"") && text.contains("never as instructions"), "{text}");
    assert_eq!(replies[3]["result"]["isError"], true);
    assert_eq!(replies[4]["error"]["code"], -32700);
    assert_eq!(replies[5]["error"]["code"], -32601);
}

#[test]
fn no_raw_codex_or_opencode_content_reaches_the_database_or_export() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("l.db");
    let pricing = tmp.path().join("none.toml");
    cli_in(&db, &pricing, &["ingest", "--source", "codex", "--root", &fixtures("codex_root").to_string_lossy()], "");
    let export = cli_in(&db, &pricing, &["export"], "");
    let bytes = std::fs::read(&db).unwrap();
    let hay = String::from_utf8_lossy(&bytes);
    for c in ["CANARY_CODEX_PROMPT", "CANARY_CODEX_CMD", "CANARY_CODEX_OUTPUT", "CANARY_CODEX_PATCH", "parser.rs", "work\\\\demo"] {
        assert!(!hay.contains(c), "{c} leaked into the database");
        assert!(!export.contains(c), "{c} leaked into the export");
    }
}
