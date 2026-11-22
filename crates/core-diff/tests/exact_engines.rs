//! Validation of the exact-match engines (plan.md §5.3, decision D8, backlog E3-S1..S4):
//! Myers against a brute-force LCS and against `imara-diff` `MyersMinimal`, Histogram against
//! `imara-diff` `Histogram`, and validity of every engine's output. `imara-diff` is a
//! dev-dependency only.

use core_base::CancelToken;
use core_diff::exact::{
    HistogramScratch, MyersScratch, PatienceScratch, histogram_matches, histogram_matches_into,
    myers_matches, myers_matches_into, patience_anchors,
};
use imara_diff::{Algorithm, Diff, Token};
use proptest::prelude::*;

// ------------------------------------------------------------------ oracles

/// Length of a longest common subsequence, O(n*m) dynamic programming.
fn lcs_len(a: &[u32], b: &[u32]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    for &x in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, &y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Match count of imara-diff with the given algorithm. Tokens are dense ids.
fn imara_matches(algo: Algorithm, a: &[u32], b: &[u32]) -> usize {
    let mut ids = std::collections::HashMap::new();
    let mut intern = |x: u32| {
        let n = ids.len() as u32;
        *ids.entry(x).or_insert(n)
    };
    let ta: Vec<Token> = a.iter().map(|&x| Token(intern(x))).collect();
    let tb: Vec<Token> = b.iter().map(|&x| Token(intern(x))).collect();
    let mut diff = Diff::default();
    diff.compute_with(algo, &ta, &tb, ids.len() as u32);
    a.len() - diff.count_removals() as usize
}

/// Matches are strictly increasing on both sides and point at equal items.
fn assert_valid(a: &[u32], b: &[u32], m: &[(u32, u32)]) {
    for w in m.windows(2) {
        assert!(
            w[0].0 < w[1].0 && w[0].1 < w[1].1,
            "not strictly increasing: {w:?}"
        );
    }
    for &(i, j) in m {
        assert_eq!(
            a[i as usize], b[j as usize],
            "pair ({i},{j}) points at different items"
        );
    }
}

fn myers(a: &[u32], b: &[u32]) -> Vec<(u32, u32)> {
    myers_matches(a, b, &CancelToken::new()).unwrap()
}

fn hist(a: &[u32], b: &[u32]) -> Vec<(u32, u32)> {
    histogram_matches(a, b, &CancelToken::new()).unwrap()
}

// ------------------------------------------------------------- generators

/// Tiny deterministic generator for the big inputs (no dependency on a rand crate).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_seq(rng: &mut Rng, n: usize, alphabet: u64) -> Vec<u32> {
    (0..n).map(|_| rng.below(alphabet) as u32).collect()
}

/// `a` with random edits: deletions, insertions, replacements.
fn edited(rng: &mut Rng, a: &[u32], edits: usize, alphabet: u64) -> Vec<u32> {
    let mut b = a.to_vec();
    for _ in 0..edits {
        if b.is_empty() {
            b.push(rng.below(alphabet) as u32);
            continue;
        }
        let at = rng.below(b.len() as u64) as usize;
        match rng.below(3) {
            0 => {
                b.remove(at);
            }
            1 => b.insert(at, rng.below(alphabet) as u32),
            _ => b[at] = rng.below(alphabet) as u32,
        }
    }
    b
}

// ------------------------------------------------------------------- Myers

#[test]
fn myers_small_cases() {
    assert!(myers(&[], &[]).is_empty());
    assert!(myers(&[1, 2], &[]).is_empty());
    assert!(myers(&[], &[1, 2]).is_empty());
    assert_eq!(myers(&[1, 2, 3], &[1, 2, 3]), vec![(0, 0), (1, 1), (2, 2)]);
    assert!(myers(&[1, 2, 3], &[4, 5, 6]).is_empty());
    // a single change in the middle
    assert_eq!(myers(&[1, 2, 3], &[1, 9, 3]), vec![(0, 0), (2, 2)]);
    // a single insertion and a single deletion
    assert_eq!(myers(&[1, 3], &[1, 2, 3]), vec![(0, 0), (1, 2)]);
    assert_eq!(myers(&[1, 2, 3], &[1, 3]), vec![(0, 0), (2, 1)]);
}

#[test]
fn myers_is_minimal_on_classic_examples() {
    // the example of the Myers paper: ABCABBA vs CBABAC has an LCS of length 4
    let a = [1, 2, 3, 1, 2, 2, 1];
    let b = [3, 2, 1, 2, 1, 3];
    let m = myers(&a, &b);
    assert_valid(&a, &b, &m);
    assert_eq!(m.len(), 4);
    assert_eq!(lcs_len(&a, &b), 4);
}

#[test]
fn myers_oracle_is_right_on_tiny_inputs() {
    assert_eq!(lcs_len(&[], &[1]), 0);
    assert_eq!(lcs_len(&[1, 2, 3], &[3, 2, 1]), 1);
    assert_eq!(lcs_len(&[1, 2, 3, 4], &[2, 4]), 2);
    assert_eq!(lcs_len(&[1, 1, 1], &[1, 1]), 2);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, ..ProptestConfig::default() })]

    /// The number of Myers matches equals the brute-force LCS length (inputs up to 200 items).
    #[test]
    fn myers_count_equals_brute_force_lcs(
        a in proptest::collection::vec(0u32..6, 0..200),
        b in proptest::collection::vec(0u32..6, 0..200),
    ) {
        let m = myers(&a, &b);
        assert_valid(&a, &b, &m);
        prop_assert_eq!(m.len(), lcs_len(&a, &b));
    }

    /// Same with a big alphabet (few repeats) and with heavy repetition.
    #[test]
    fn myers_count_equals_lcs_for_other_alphabets(
        a in proptest::collection::vec(0u32..1000, 0..120),
        b in proptest::collection::vec(0u32..2, 0..120),
        c in proptest::collection::vec(0u32..1000, 0..120),
    ) {
        for (x, y) in [(&a, &c), (&b, &b), (&a, &b), (&c, &a)] {
            let m = myers(x, y);
            assert_valid(x, y, &m);
            prop_assert_eq!(m.len(), lcs_len(x, y));
        }
    }

    /// Every engine's output is valid, and Histogram never beats the minimum.
    #[test]
    fn histogram_output_is_valid_and_not_above_the_minimum(
        a in proptest::collection::vec(0u32..8, 0..150),
        b in proptest::collection::vec(0u32..8, 0..150),
    ) {
        let m = hist(&a, &b);
        assert_valid(&a, &b, &m);
        prop_assert!(m.len() <= lcs_len(&a, &b));
    }
}

#[test]
fn myers_equals_imara_minimal_on_large_inputs() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // (length, edits, alphabet)
    for &(n, edits, alphabet) in &[
        (2_000usize, 50usize, 40u64),
        (5_000, 300, 5),
        (8_000, 40, 100_000),
        (3_000, 1_500, 3),
        (6_000, 0, 10),
        (4_000, 4_000, 2),
    ] {
        let a = random_seq(&mut rng, n, alphabet);
        let b = edited(&mut rng, &a, edits, alphabet);
        let ours = myers(&a, &b);
        assert_valid(&a, &b, &ours);
        let theirs = imara_matches(Algorithm::MyersMinimal, &a, &b);
        assert_eq!(
            ours.len(),
            theirs,
            "n={n} edits={edits} alphabet={alphabet}"
        );
    }
}

#[test]
fn myers_handles_adversarial_shapes() {
    let mut rng = Rng(42);
    let cases: Vec<(&str, Vec<u32>, Vec<u32>)> = vec![
        (
            "long identical runs",
            (0..3000).map(|i| i / 100).collect(),
            (0..3000).map(|i| i / 100).collect(),
        ),
        ("reversed", (0..1500).collect(), (0..1500).rev().collect()),
        ("one side empty", (0..500).collect(), vec![]),
        ("everything equal", vec![7; 800], vec![7; 600]),
        (
            "two symbols alternating",
            (0..2000).map(|i| i % 2).collect(),
            (0..2000).map(|i| (i + 1) % 2).collect(),
        ),
        ("disjoint", (0..1000).collect(), (1000..2000).collect()),
        (
            "random binary",
            random_seq(&mut rng, 2000, 2),
            random_seq(&mut rng, 1800, 2),
        ),
    ];
    for (name, a, b) in cases {
        let ours = myers(&a, &b);
        assert_valid(&a, &b, &ours);
        let theirs = imara_matches(Algorithm::MyersMinimal, &a, &b);
        assert_eq!(ours.len(), theirs, "{name}");
    }
}

#[test]
fn myers_can_be_cancelled() {
    let mut rng = Rng(7);
    let a = random_seq(&mut rng, 4000, 2);
    let b = random_seq(&mut rng, 4000, 2);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(myers_matches(&a, &b, &cancel).unwrap_err().is_cancelled());
}

// --------------------------------------------------------------- Histogram

#[test]
fn histogram_small_cases() {
    assert!(hist(&[], &[]).is_empty());
    assert_eq!(hist(&[1, 2, 3], &[1, 2, 3]), vec![(0, 0), (1, 1), (2, 2)]);
    assert_eq!(hist(&[1, 2, 3], &[1, 9, 3]), vec![(0, 0), (2, 2)]);
    // the least common item is the anchor: 9 occurs once on each side
    let a = [1, 1, 1, 9, 1, 1];
    let b = [1, 1, 9, 1];
    let m = hist(&a, &b);
    assert_valid(&a, &b, &m);
    assert!(
        m.contains(&(3, 2)),
        "the unique item anchors the match: {m:?}"
    );
}

#[test]
fn histogram_falls_back_to_myers_when_everything_is_common() {
    // every item occurs 100 times (> 64): no usable anchor, Myers decides, so the result is
    // minimal
    let a: Vec<u32> = (0..400).map(|i| i % 4).collect();
    let b: Vec<u32> = (0..400).map(|i| (i + 1) % 4).collect();
    let m = hist(&a, &b);
    assert_valid(&a, &b, &m);
    assert_eq!(m.len(), lcs_len(&a, &b), "the fallback to Myers is exact");
}

#[test]
fn candidates_with_more_than_64_copies_are_never_anchors_decision_d24() {
    // 65 copies of 1 in `a` (the Delphi code would still accept exactly 65), plus a prefix
    // and suffix that differ so the range is not trimmed away. Result stays minimal because
    // the only candidate is rejected and Myers runs.
    let mut a = vec![100u32];
    a.extend(std::iter::repeat_n(1, 65));
    a.push(200);
    let mut b = vec![300u32];
    b.extend(std::iter::repeat_n(1, 40));
    b.push(400);
    let m = hist(&a, &b);
    assert_valid(&a, &b, &m);
    assert_eq!(m.len(), lcs_len(&a, &b));
}

#[test]
fn histogram_count_is_within_two_percent_of_imara_on_the_random_corpus() {
    let mut rng = Rng(0xDEAD_BEEF);
    let mut report = String::new();
    let (mut total_ours, mut total_theirs, mut worst) = (0usize, 0usize, 0.0f64);
    for round in 0..60 {
        let n = 500 + (rng.below(4000) as usize);
        let alphabet = [3u64, 10, 50, 500, 100_000][(round % 5) as usize];
        let edits = (n / (5 + rng.below(40) as usize)).max(1);
        let a = random_seq(&mut rng, n, alphabet);
        let b = edited(&mut rng, &a, edits, alphabet);
        let ours = hist(&a, &b);
        assert_valid(&a, &b, &ours);
        let theirs = imara_matches(Algorithm::Histogram, &a, &b);
        total_ours += ours.len();
        total_theirs += theirs;
        let gap = (theirs as f64 - ours.len() as f64) / theirs.max(1) as f64;
        worst = worst.max(gap);
        if gap.abs() > 0.02 {
            report += &format!(
                "round {round}: n={n} alphabet={alphabet} edits={edits}: ours {} imara {theirs} ({:+.2}%)\n",
                ours.len(),
                -gap * 100.0
            );
        }
    }
    // Larger gaps are logged for review, never silently accepted (plan.md §5.3).
    if !report.is_empty() {
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("histogram-gaps.txt");
        std::fs::write(&path, &report).unwrap();
        eprintln!(
            "histogram gaps above 2% written to {}:\n{report}",
            path.display()
        );
    }
    let aggregate = (total_theirs as f64 - total_ours as f64).abs() / total_theirs as f64;
    assert!(
        aggregate <= 0.02,
        "aggregate match count differs by {:.2}% from imara-diff",
        aggregate * 100.0
    );
    assert!(
        worst <= 0.10,
        "a single case is {:.1}% below imara-diff; see the report",
        worst * 100.0
    );
}

// ---------------------------------------------------------------- Patience

#[test]
fn patience_anchor_properties() {
    let mut rng = Rng(99);
    let mut scratch = PatienceScratch::new();
    for _ in 0..200 {
        let a = random_seq(&mut rng, 80, 30);
        let b = edited(&mut rng, &a, 20, 30);
        let mut out = Vec::new();
        patience_anchors(
            &a,
            &b,
            (0, a.len()),
            (0, b.len()),
            |_| true,
            |_| true,
            &mut scratch,
            &mut out,
        );
        assert_valid(&a, &b, &out);
        for &(i, j) in &out {
            let ca = a.iter().filter(|&&x| x == a[i as usize]).count();
            let cb = b.iter().filter(|&&x| x == b[j as usize]).count();
            assert_eq!((ca, cb), (1, 1), "anchors are unique in both");
        }
    }
}

/// Decision D39: a long equal run is scanned once. Every token occurs about 33 times (below the
/// 64 limit), so no Myers fallback hides the cost; the quadratic version never finishes.
#[test]
fn histogram_scans_a_long_equal_run_once() {
    let a: Vec<u32> = (0..100_000u32).map(|i| i % 3_000).collect();
    let mut b = a.clone();
    b[50_000] = 999_999;
    let started = std::time::Instant::now();
    let m = hist(&a, &b);
    assert_valid(&a, &b, &m);
    assert!(m.len() >= a.len() - 2, "{} matches", m.len());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// One scratch used for many calls of different sizes, and after a cancelled call, gives the
/// same matches as a fresh scratch (heads/stamps are reset, `pre`/`tmp` are restored on the
/// error path).
#[test]
fn scratch_reuse_gives_the_same_answers_as_fresh_scratch() {
    let mut rng = Rng(7);
    let (mut ms, mut hs) = (MyersScratch::new(), HistogramScratch::new());
    let cancelled = CancelToken::new();
    cancelled.cancel();
    for round in 0..40 {
        let n = [0usize, 1, 5, 200, 3_000, 50][round % 6];
        let alphabet = [3u64, 50, 1_000_000][round % 3];
        let a = random_seq(&mut rng, n, alphabet);
        let b = edited(&mut rng, &a, n / 7 + 1, alphabet);
        if round % 5 == 4 {
            // a cancelled call in the middle must not poison the next one
            let mut sink = Vec::new();
            // trivial inputs finish before the first cancel check, so either outcome is fine
            let _ = myers_matches_into(&a, &b, &cancelled, &mut ms, &mut sink);
            let mut sink = Vec::new();
            assert!(
                histogram_matches_into(&a, &b, &cancelled, &mut hs, &mut sink).is_err() || n < 3
            );
        }
        let mut got = Vec::new();
        myers_matches_into(&a, &b, &CancelToken::new(), &mut ms, &mut got).unwrap();
        assert_eq!(got, myers(&a, &b), "myers round {round}");
        let mut got = Vec::new();
        histogram_matches_into(&a, &b, &CancelToken::new(), &mut hs, &mut got).unwrap();
        assert_eq!(got, hist(&a, &b), "histogram round {round}");
    }
}

/// The reduction in front of the engines: ends that overlap, one side empty after the trim,
/// one-sided lines everywhere.
#[test]
fn prefilter_edge_cases_through_the_engines() {
    let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
        (vec![1, 2, 3], vec![1, 2, 3]),
        (vec![1, 2, 3], vec![1, 3]),
        (vec![1, 3], vec![1, 2, 3]),
        (vec![1, 1, 1], vec![1, 1]),
        (vec![1, 2, 1, 2], vec![2, 1, 2, 1]),
        (vec![9, 8, 7], vec![1, 2, 3]),
        (vec![], vec![1]),
        (vec![5, 1, 2, 5], vec![5, 5]),
        (vec![1, 9, 2], vec![1, 8, 2]),
    ];
    for (a, b) in cases {
        let (m, h) = (myers(&a, &b), hist(&a, &b));
        assert_valid(&a, &b, &m);
        assert_valid(&a, &b, &h);
        assert_eq!(m.len(), lcs_len(&a, &b), "{a:?} vs {b:?}");
        assert!(h.len() <= m.len());
    }
}
