use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use lightning::adapter::claude_code::ClaudeCode;
use lightning::doctor::{DoctorEnv, build, build_with};
use lightning::ingest::ingest;
use lightning::pricing::PriceTable;
use lightning::store::Store;

fn setup() -> Store {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_root");
    let mut store = Store::open_in_memory().unwrap();
    ingest(&mut store, &ClaudeCode, &root).unwrap();
    store
}

#[test]
fn doctor_golden() {
    let store = setup();
    let now = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap();
    let r = build(&store, &PriceTable::builtin(), now).unwrap();
    let expected = "\
Lightning doctor

data: 3 files, 2 sessions, 10 turns, 8 tool calls
turns by harness: claude_code=10
parse: 1 malformed lines, 1 file(s) ending in a partial line
range: 2026-10-01T10:00:00.000Z .. 2026-10-02T09:00:04.000Z
other line types (ignored): system=1, weird-new-type=1

field presence by harness version (non-synthetic turns)
harness      version        turns  usage  cache_read  cache_write  stop_reason
claude_code  2.9.0              3   100%         33%          33%          67%
claude_code  3.0.1              6   100%        100%         100%         100%

pricing: version 2026-10-12, unpriced: claude-haiku-9, claude-opus-5, claude-sonnet-5, claude-sonnet-5-5

warnings
- 1 malformed line(s) skipped; if this grows after a harness update the format may have changed
- 4 model(s) have no complete price and are reported as unpriced: claude-haiku-9, claude-opus-5, claude-sonnet-5, claude-sonnet-5-5
";
    assert_eq!(r.render(), expected);
}

#[test]
fn retention_and_stale_pricing_warnings() {
    let store = setup();
    let now = Utc.with_ymd_and_hms(2027, 1, 15, 0, 0, 0).unwrap();
    let r = build(&store, &PriceTable::builtin(), now).unwrap();
    let text = r.render();
    assert!(text.contains("pricing table is 95 days old"), "{text}");
    assert!(text.contains("oldest ingested session started 105 days ago"), "{text}");
    assert!(text.contains("after 30 days (cleanupPeriodDays)"));
}

#[test]
fn retention_uses_the_configured_cleanup_period_and_counts_archived_files() {
    let store = setup();
    store.conn().execute("INSERT INTO files(path, size, mtime_ns, head_hash, ingested_at) VALUES ('/gone/x.jsonl', 1, 1, '', 't')", []).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 20, 0, 0, 0).unwrap();
    let env = DoctorEnv { claude_cleanup_days: 365, check_files: true };
    let r = build_with(&store, &PriceTable::builtin(), now, &env).unwrap();
    assert_eq!(r.archived_files, 1);
    assert!(!r.render().contains("cleanupPeriodDays"), "50 days is fine under a 365 day period");
    let r = build_with(&store, &PriceTable::builtin(), now, &DoctorEnv::default()).unwrap();
    assert!(r.render().contains("after 30 days"));
}

#[test]
fn empty_database_does_not_fail() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap();
    let r = build(&store, &PriceTable::builtin(), now).unwrap();
    assert_eq!(r.turns, 0);
    assert!(r.render().contains("range: n/a .. n/a"));
}
