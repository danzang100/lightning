//! Gate checks on synthetic data with planted ground truth (spec 004, W13).
use lightning::adapter::claude_code::ClaudeCode;
use lightning::analysis::{self, PairKey, Verdict};
use lightning::ingest::ingest;
use lightning::segment::{self, Segment, SegmentOptions};
use lightning::store::Store;
use lightning::synth::{self, Plan, PairSpec, Truth};

pub fn run(plan: &Plan) -> (Store, Vec<Segment>, Vec<Truth>) {
    let tmp = tempfile::tempdir().unwrap();
    let truth = synth::generate(tmp.path(), plan).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    ingest(&mut store, &ClaudeCode, tmp.path()).unwrap();
    let models: Vec<&str> = plan.pairs.iter().map(|p| p.model.as_str()).collect();
    let segs = segment::build(&store, &synth::test_pricing(&models), &SegmentOptions::default()).unwrap();
    (store, segs, truth)
}

fn plan(pairs: Vec<PairSpec>, tasks: Vec<&'static str>) -> Plan {
    Plan { pairs, tasks, ..Plan::default() }
}

#[test]
fn heuristics_agree_with_planted_labels() {
    let tasks = vec!["debugging", "refactor", "feature", "test_writing", "exploration", "config_devops", "docs"];
    let (_, segs, truth) = run(&plan(vec![PairSpec::new("model-x", 0.7, 1.0)], tasks));
    assert_eq!(segs.len(), truth.len(), "one segment per generated session");
    let (mut task_ok, mut out_ok, mut out_n) = (0, 0, 0);
    for t in &truth {
        let s = segs.iter().find(|s| s.session_id == t.session_id).unwrap();
        task_ok += (s.task_type == t.task) as usize;
        if let Some(ok) = s.success() {
            out_n += 1;
            out_ok += (ok == t.success) as usize;
        }
    }
    let task_rate = task_ok as f64 / truth.len() as f64;
    let out_rate = out_ok as f64 / out_n as f64;
    assert!(task_rate >= 0.8, "task agreement {task_rate}");
    assert!(out_rate >= 0.75, "outcome agreement {out_rate} over {out_n}");
    assert!(out_n as f64 >= 0.9 * truth.len() as f64, "most planted outcomes are decided ({out_n})");
}

#[test]
fn planted_cheaper_equal_pair_is_recommended() {
    // Equal planted success still varies by sample, so this is a power check over seeds.
    let mut found = Vec::new();
    for seed in [11, 12, 13, 14, 15] {
        let pairs = vec![PairSpec::new("big-model", 0.9, 1.0), PairSpec::new("small-model", 0.9, 0.3)];
        let (_, segs, _) = run(&Plan { segments: 200, seed, ..plan(pairs, vec!["debugging"]) });
        let f = analysis::findings(&segs, PairKey::Pair, 20);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].pair_a, "claude_code/big-model");
        assert_eq!(f[0].pair_b, "claude_code/small-model");
        assert_eq!(f[0].cost_basis, "usd");
        assert_ne!(f[0].verdict, Verdict::Worse, "{}", f[0].summary);
        found.push(f[0].verdict == Verdict::Recommend);
    }
    assert!(found.iter().filter(|x| **x).count() >= 4, "planted effect found in {found:?}");
}

#[test]
fn planted_null_yields_no_recommendation() {
    for seed in [1, 2, 3, 4, 5] {
        let pairs = vec![PairSpec::new("model-a", 0.8, 1.0), PairSpec::new("model-b", 0.8, 1.0)];
        let (_, segs, _) = run(&Plan { seed, ..plan(pairs, vec!["debugging", "refactor"]) });
        for f in analysis::findings(&segs, PairKey::Pair, 20) {
            assert_ne!(f.verdict, Verdict::Recommend, "seed {seed}: {}", f.summary);
        }
    }
}

#[test]
fn planted_worse_cheap_pair_is_not_recommended() {
    let pairs = vec![PairSpec::new("big-model", 0.95, 1.0), PairSpec::new("small-model", 0.45, 0.3)];
    let (_, segs, _) = run(&plan(pairs, vec!["debugging"]));
    let f = analysis::findings(&segs, PairKey::Pair, 20);
    assert_eq!(f[0].verdict, Verdict::Worse, "{}", f[0].summary);
}

#[test]
fn small_samples_are_insufficient() {
    let pairs = vec![PairSpec::new("big-model", 0.9, 1.0), PairSpec::new("small-model", 0.9, 0.3)];
    let (_, segs, _) = run(&Plan { segments: 8, ..plan(pairs, vec!["debugging"]) });
    assert!(analysis::findings(&segs, PairKey::Pair, 20).is_empty());
    let a: Vec<&Segment> = segs.iter().filter(|s| s.model == "big-model").collect();
    let b: Vec<&Segment> = segs.iter().filter(|s| s.model == "small-model").collect();
    let f = analysis::compare_pairs("debugging", "a", &a, "b", &b, 20);
    assert_eq!(f.verdict, Verdict::InsufficientData);
}

#[test]
fn recommend_pair_ranks_for_the_described_task() {
    let pairs = vec![PairSpec::new("big-model", 0.9, 1.0), PairSpec::new("small-model", 0.5, 0.3)];
    let (_, segs, _) = run(&plan(pairs, vec!["debugging"]));
    let r = analysis::recommend(&segs, "fix the crash when the config is missing", None, 20);
    assert_eq!(r.task_type, "debugging");
    assert_eq!(r.verdict, "recommend");
    assert_eq!(r.ranking[0].pair, "claude_code/big-model");
}
