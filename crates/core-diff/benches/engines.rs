//! Speed of the exact engines against `imara-diff` (backlog E3-S4.3, plan.md §5.3, decision D8):
//! ours must stay within 1.5x of imara on 100k-line inputs. `imara-diff` is a dev-dependency.
//!
//! Run: `cargo bench -p core-diff --bench engines`. Prints a table and fails when a ratio is
//! above 1.5. Best of five runs per cell.

use std::hint::black_box;
use std::time::Instant;

use core_base::CancelToken;
use core_diff::exact::{histogram_matches, myers_matches};
use imara_diff::{Algorithm, Diff, Token};

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

fn edited(rng: &mut Rng, a: &[u32], edits: usize, alphabet: u64) -> Vec<u32> {
    let mut b = a.to_vec();
    for _ in 0..edits {
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

fn best_of(n: usize, mut f: impl FnMut()) -> f64 {
    (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

fn imara(algo: Algorithm, a: &[u32], b: &[u32], ids: u32) -> usize {
    let ta: Vec<Token> = a.iter().map(|&x| Token(x)).collect();
    let tb: Vec<Token> = b.iter().map(|&x| Token(x)).collect();
    let mut d = Diff::default();
    d.compute_with(algo, &ta, &tb, ids);
    a.len() - d.count_removals() as usize
}

fn main() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let n = 100_000;
    // (name, alphabet, edits)
    let shapes = [
        ("log-like, 200 edits", 1_000_000u64, 200usize),
        ("log-like, 5000 edits", 1_000_000, 5_000),
        ("code-like, 2000 edits", 5_000, 2_000),
        ("few symbols, 500 edits", 8, 500),
    ];
    println!(
        "{:<26} {:>12} {:>12} {:>7}",
        "100k lines", "ours (s)", "imara (s)", "ratio"
    );
    let mut failed = false;
    for (name, alphabet, edits) in shapes {
        let a = random_seq(&mut rng, n, alphabet);
        let b = edited(&mut rng, &a, edits, alphabet);
        let ids = alphabet as u32 + 1;
        let cancel = CancelToken::new();
        for (label, ours_fn, algo) in [
            ("myers", 0, Algorithm::MyersMinimal),
            ("histogram", 1, Algorithm::Histogram),
        ] {
            let ours = best_of(5, || {
                let m = if ours_fn == 0 {
                    myers_matches(&a, &b, &cancel)
                } else {
                    histogram_matches(&a, &b, &cancel)
                };
                black_box(m.unwrap());
            });
            let theirs = best_of(5, || {
                black_box(imara(algo, &a, &b, ids));
            });
            let ratio = ours / theirs;
            let flag = if ratio > 1.5 { "  <-- over 1.5x" } else { "" };
            failed |= ratio > 1.5;
            println!(
                "{:<26} {:>12.4} {:>12.4} {:>7.2}{flag}",
                format!("{name} / {label}"),
                ours,
                theirs,
                ratio
            );
        }
    }
    assert!(
        !failed,
        "an engine is more than 1.5x slower than imara-diff"
    );
}
