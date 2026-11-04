//! Suggestions, apply, rollback and review on planted data (spec 004, W10 and W13).
use chrono::{Duration, TimeZone, Utc};
use lightning::adapter::claude_code::ClaudeCode;
use lightning::adopt::{self, Ctx};
use lightning::ingest::ingest;
use lightning::segment::{self, Segment, SegmentOptions};
use lightning::store::Store;
use lightning::suggest::{self, Kind, Target};
use lightning::synth::{self, PairSpec, Plan};

const ALL: [Kind; 4] = [Kind::Default, Kind::Subagent, Kind::Rule, Kind::Hygiene];

fn build(plan: &Plan, store: &mut Store, root: &std::path::Path) -> Vec<Segment> {
    synth::generate(root, plan).unwrap();
    ingest(store, &ClaudeCode, root).unwrap();
    let models: Vec<&str> = plan.pairs.iter().map(|p| p.model.as_str()).collect();
    segment::build(store, &synth::test_pricing(&models), &SegmentOptions::default()).unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap()
}

#[test]
fn planted_rule_violation_targets_only_that_pair() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let mut b = PairSpec::new("model-b", 0.8, 1.0);
    b.reread_p = 0.6;
    let plan = Plan { pairs: vec![PairSpec::new("model-a", 0.8, 1.0), b], segments: 60, ..Plan::default() };
    let segs = build(&plan, &mut store, tmp.path());
    let rules: Vec<_> = suggest::generate(&segs, &[], &[Kind::Rule], 20, now()).into_iter().collect();
    assert_eq!(rules.len(), 1, "{rules:#?}");
    assert_eq!(rules[0].scope.pair_from.as_deref(), Some("claude_code/model-b"));
    assert_eq!(rules[0].scope.metric.as_deref(), Some("reread"));
    let Target::User { rel } = &rules[0].change.as_ref().unwrap().target else { panic!() };
    assert_eq!(rel, ".claude/CLAUDE.md");
}

#[test]
fn planted_null_yields_no_suggestions() {
    for seed in [21, 22, 23] {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let plan = Plan { pairs: vec![PairSpec::new("model-a", 0.8, 1.0), PairSpec::new("model-b", 0.8, 1.0)], segments: 60, seed, ..Plan::default() };
        let segs = build(&plan, &mut store, tmp.path());
        let s = suggest::generate(&segs, &[], &ALL, 20, now());
        assert!(s.is_empty(), "seed {seed}: {:#?}", s.iter().map(|x| &x.rationale).collect::<Vec<_>>());
    }
}

#[test]
fn default_pair_apply_rollback_and_review() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(repos.join("alpha")).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let plan = Plan {
        pairs: vec![PairSpec::new("big-model", 0.9, 1.0), PairSpec::new("small-model", 0.9, 0.3)],
        repos: vec!["alpha".into()],
        segments: 200,
        seed: 31,
        cwd_root: repos.to_string_lossy().into_owned(),
        ..Plan::default()
    };
    let segs = build(&plan, &mut store, &tmp.path().join("logs"));
    let fresh = suggest::generate(&segs, &[], &[Kind::Default], 20, now());
    let saved = suggest::save(&store, fresh, now()).unwrap();
    let repo_level = saved.iter().find(|s| s.change.is_some()).expect("a repo-level default suggestion");
    assert_eq!(repo_level.scope.pair_to.as_deref(), Some("claude_code/small-model"));
    assert!(saved.iter().any(|s| s.change.is_none() && s.scope.task.as_deref() == Some("debugging")), "task-level advice too");

    // Saving the same evidence again does not duplicate.
    let again = suggest::save(&store, suggest::generate(&segs, &[], &[Kind::Default], 20, now()), now()).unwrap();
    assert_eq!(again.iter().map(|s| &s.id).collect::<Vec<_>>(), saved.iter().map(|s| &s.id).collect::<Vec<_>>());

    let home = tmp.path().join("home");
    let repo = repos.join("alpha");
    let settings = repo.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{\n  \"model\": \"big-model\",\n  \"env\": {}\n}\n").unwrap();
    let id = repo_level.id.clone();

    // Wrong repo is refused; declining writes nothing.
    let wrong = Ctx { home: home.clone(), repo: Some(tmp.path().to_path_buf()) };
    assert!(adopt::apply(&store, &id, &wrong, true, &mut &b""[..], &mut Vec::new(), now()).is_err());
    let ctx = Ctx { home: home.clone(), repo: Some(repo.clone()) };
    let mut out = Vec::new();
    assert!(!adopt::apply(&store, &id, &ctx, false, &mut &b"n\n"[..], &mut out, now()).unwrap());
    assert!(std::fs::read_to_string(&settings).unwrap().contains("big-model"));
    let shown = String::from_utf8(out).unwrap();
    assert!(shown.contains("-  \"model\": \"big-model\",\n+  \"model\": \"small-model\","), "{shown}");

    assert!(adopt::apply(&store, &id, &ctx, false, &mut &b"y\n"[..], &mut Vec::new(), now()).unwrap());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\n  \"model\": \"small-model\",\n  \"env\": {}\n}\n");
    assert_eq!(suggest::get(&store, &id).unwrap().status, "applied");

    // Review right after adoption: nothing after yet.
    let r = adopt::review(&store, &segs, &[], now()).unwrap();
    assert_eq!(r[0].verdict, "insufficient_data");

    // A hand edit blocks rollback unless forced.
    std::fs::write(&settings, "{\"model\": \"other\"}").unwrap();
    assert!(adopt::rollback(&store, &id, true, false, &mut &b""[..], &mut Vec::new(), now()).is_err());
    assert!(adopt::rollback(&store, &id, true, true, &mut &b""[..], &mut Vec::new(), now() + Duration::minutes(1)).unwrap());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\n  \"model\": \"big-model\",\n  \"env\": {}\n}\n");
    assert_eq!(suggest::get(&store, &id).unwrap().status, "rolled_back");
}

#[test]
fn rule_applies_to_user_file_and_rollback_removes_a_created_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let mut b = PairSpec::new("model-b", 0.8, 1.0);
    b.reread_p = 0.7;
    let plan = Plan { pairs: vec![PairSpec::new("model-a", 0.8, 1.0), b], segments: 60, ..Plan::default() };
    let segs = build(&plan, &mut store, &tmp.path().join("logs"));
    let saved = suggest::save(&store, suggest::generate(&segs, &[], &[Kind::Rule], 20, now()), now()).unwrap();
    let home = tmp.path().join("home");
    let ctx = Ctx { home: home.clone(), repo: None };
    assert!(adopt::apply(&store, &saved[0].id, &ctx, true, &mut &b""[..], &mut Vec::new(), now()).unwrap());
    let text = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    assert!(text.contains("<!-- lightning:rule:model-b:reread -->") && text.contains("## When running as model-b"), "{text}");
    assert!(adopt::rollback(&store, &saved[0].id, true, false, &mut &b""[..], &mut Vec::new(), now()).unwrap());
    assert!(!home.join(".claude/CLAUDE.md").exists());
}

#[test]
fn expired_and_dismissed_suggestions_stay_put() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let mut b = PairSpec::new("model-b", 0.8, 1.0);
    b.reread_p = 0.7;
    let plan = Plan { pairs: vec![PairSpec::new("model-a", 0.8, 1.0), b], segments: 60, ..Plan::default() };
    let segs = build(&plan, &mut store, tmp.path());
    let saved = suggest::save(&store, suggest::generate(&segs, &[], &[Kind::Rule], 20, now()), now()).unwrap();
    suggest::set_status(&store, &saved[0].id, "dismissed", now()).unwrap();
    let again = suggest::save(&store, suggest::generate(&segs, &[], &[Kind::Rule], 20, now()), now()).unwrap();
    assert!(again.is_empty());
    assert_eq!(suggest::list(&store, Some("dismissed")).unwrap().len(), 1);
}
