//! Plain, explainable statistics (lightning-spec 9.5, 14): Wilson intervals for rates and a
//! stratified bootstrap with a fixed seed for differences between pairs. No opaque models.
use std::collections::BTreeMap;

use serde::Serialize;

pub const Z95: f64 = 1.959_963_985;

/// Wilson score interval for `k` successes out of `n`.
pub fn wilson(k: u64, n: u64) -> Option<(f64, f64)> {
    if n == 0 {
        return None;
    }
    let (k, n, z) = (k as f64, n as f64, Z95);
    let p = k / n;
    let denom = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denom;
    let half = z * ((p * (1.0 - p) + z * z / (4.0 * n)) / n).sqrt() / denom;
    Some(((centre - half).max(0.0), (centre + half).min(1.0)))
}

/// SplitMix64: small, deterministic, good enough for resampling.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

pub fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    Some(if s.len().is_multiple_of(2) { (s[m - 1] + s[m]) / 2.0 } else { s[m] })
}

pub fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// One observation for a comparison: its stratum, its outcome (None = unknown) and its cost.
#[derive(Debug, Clone)]
pub struct Obs {
    pub stratum: String,
    pub success: Option<bool>,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Interval {
    pub value: f64,
    pub lo: f64,
    pub hi: f64,
}

/// B relative to A, stratified: success difference (B - A) and cost ratio (B / A).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PairDiff {
    pub success_diff: Option<Interval>,
    pub cost_ratio: Option<Interval>,
    /// True when estimates use only strata where both pairs appear.
    pub matched: bool,
    pub strata_used: usize,
}

struct Stratum<'a> {
    a: Vec<&'a Obs>,
    b: Vec<&'a Obs>,
}

fn estimate(strata: &[(Vec<&Obs>, Vec<&Obs>)]) -> (Option<f64>, Option<f64>) {
    let (mut wd, mut sd, mut wc, mut ca, mut cb) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (a, b) in strata {
        let da: Vec<bool> = a.iter().filter_map(|o| o.success).collect();
        let db: Vec<bool> = b.iter().filter_map(|o| o.success).collect();
        if !da.is_empty() && !db.is_empty() {
            let w = (da.len() * db.len()) as f64 / (da.len() + db.len()) as f64;
            let pa = da.iter().filter(|x| **x).count() as f64 / da.len() as f64;
            let pb = db.iter().filter(|x| **x).count() as f64 / db.len() as f64;
            wd += w;
            sd += w * (pb - pa);
        }
        if !a.is_empty() && !b.is_empty() {
            let w = (a.len() * b.len()) as f64 / (a.len() + b.len()) as f64;
            wc += w;
            ca += w * a.iter().map(|o| o.cost).sum::<f64>() / a.len() as f64;
            cb += w * b.iter().map(|o| o.cost).sum::<f64>() / b.len() as f64;
        }
    }
    let diff = (wd > 0.0).then(|| sd / wd);
    let ratio = (wc > 0.0 && ca > 0.0).then(|| cb / ca);
    (diff, ratio)
}

/// Stratified comparison with a percentile bootstrap. Intervals are 90% (5th to 95th percentile),
/// so each bound is a one-sided 5% test, as usual for non-inferiority. Strata where only one pair
/// appears are dropped (that is the matching); if none are shared, everything is pooled and
/// `matched` is false.
pub fn compare(a: &[Obs], b: &[Obs], reps: usize, seed: u64) -> PairDiff {
    let mut by: BTreeMap<&str, Stratum> = BTreeMap::new();
    for o in a {
        by.entry(o.stratum.as_str()).or_insert_with(|| Stratum { a: vec![], b: vec![] }).a.push(o);
    }
    for o in b {
        by.entry(o.stratum.as_str()).or_insert_with(|| Stratum { a: vec![], b: vec![] }).b.push(o);
    }
    let mut strata: Vec<(Vec<&Obs>, Vec<&Obs>)> =
        by.into_values().filter(|s| !s.a.is_empty() && !s.b.is_empty()).map(|s| (s.a, s.b)).collect();
    let matched = !strata.is_empty();
    if !matched {
        strata = vec![(a.iter().collect(), b.iter().collect())];
    }
    let (diff, ratio) = estimate(&strata);
    let mut rng = Rng::new(seed);
    let (mut diffs, mut ratios) = (Vec::with_capacity(reps), Vec::with_capacity(reps));
    for _ in 0..reps {
        let sample: Vec<(Vec<&Obs>, Vec<&Obs>)> = strata
            .iter()
            .map(|(sa, sb)| {
                let ra = (0..sa.len()).map(|_| sa[rng.below(sa.len())]).collect();
                let rb = (0..sb.len()).map(|_| sb[rng.below(sb.len())]).collect();
                (ra, rb)
            })
            .collect();
        let (d, r) = estimate(&sample);
        if let Some(d) = d {
            diffs.push(d);
        }
        if let Some(r) = r {
            ratios.push(r);
        }
    }
    diffs.sort_by(f64::total_cmp);
    ratios.sort_by(f64::total_cmp);
    let interval = |point: Option<f64>, boot: &[f64]| {
        point.filter(|_| !boot.is_empty()).map(|v| Interval { value: v, lo: percentile(boot, 0.05), hi: percentile(boot, 0.95) })
    };
    PairDiff {
        success_diff: interval(diff, &diffs),
        cost_ratio: interval(ratio, &ratios),
        matched,
        strata_used: strata.len(),
    }
}

/// Two independent proportions, Newcombe's hybrid score interval for p2 - p1.
pub fn newcombe(k1: u64, n1: u64, k2: u64, n2: u64) -> Option<Interval> {
    let (l1, u1) = wilson(k1, n1)?;
    let (l2, u2) = wilson(k2, n2)?;
    let p1 = k1 as f64 / n1 as f64;
    let p2 = k2 as f64 / n2 as f64;
    let d = p2 - p1;
    let lo = d - ((p2 - l2).powi(2) + (u1 - p1).powi(2)).sqrt();
    let hi = d + ((u2 - p2).powi(2) + (p1 - l1).powi(2)).sqrt();
    Some(Interval { value: d, lo, hi })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_matches_known_values() {
        let (lo, hi) = wilson(8, 10).unwrap();
        assert!((lo - 0.4902).abs() < 1e-3 && (hi - 0.9433).abs() < 1e-3, "{lo} {hi}");
        assert_eq!(wilson(0, 0), None);
        let (lo, _) = wilson(0, 5).unwrap();
        assert_eq!(lo, 0.0);
    }

    #[test]
    fn rng_is_deterministic() {
        let a: Vec<u64> = (0..3).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
        let b: Vec<u64> = (0..3).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
        assert_eq!(a, b);
    }

    fn obs(stratum: &str, n: usize, succ: usize, cost: f64) -> Vec<Obs> {
        (0..n).map(|i| Obs { stratum: stratum.into(), success: Some(i < succ), cost }).collect()
    }

    #[test]
    fn compare_detects_a_cheaper_equal_pair_and_not_a_null() {
        let a = obs("s", 40, 36, 1.0);
        let b = obs("s", 40, 36, 0.3);
        let d = compare(&a, &b, 500, 1);
        let r = d.cost_ratio.unwrap();
        assert!((r.value - 0.3).abs() < 1e-9 && r.hi < 0.9);
        let s = d.success_diff.unwrap();
        assert!(s.lo < 0.0 && s.hi > 0.0 && s.value.abs() < 1e-9);
        assert!(d.matched);
    }

    #[test]
    fn compare_matches_on_strata_and_falls_back_when_disjoint() {
        let mut a = obs("easy", 20, 20, 1.0);
        a.extend(obs("hard", 5, 1, 1.0));
        let mut b = obs("easy", 5, 5, 1.0);
        b.extend(obs("hard", 20, 4, 1.0));
        // Unmatched pooling would show B far worse; within strata they are the same.
        let d = compare(&a, &b, 300, 2);
        assert!(d.success_diff.unwrap().value.abs() < 1e-9);
        let d = compare(&obs("x", 5, 5, 1.0), &obs("y", 5, 5, 2.0), 100, 3);
        assert!(!d.matched);
        assert!((d.cost_ratio.unwrap().value - 2.0).abs() < 1e-9);
    }

    #[test]
    fn newcombe_brackets_the_difference() {
        let i = newcombe(10, 50, 30, 50).unwrap();
        assert!((i.value - 0.4).abs() < 1e-9 && i.lo > 0.0 && i.hi < 0.6);
    }
}
