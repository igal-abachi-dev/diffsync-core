//! Fuzz target `align_small_skew` (backlog E2-S9.5): random small inputs with a skew of 1, 2, 3
//! or 7, which stresses window edges. The aligner must not panic and its ops must cover every
//! line of both sides exactly once, in order.
//!
//! Input layout: byte 0 picks the skew, byte 1 the options, the rest are line tokens:
//! `0xFF` ends the left text, every other byte picks a line from a small pool (so repeats and
//! runs are common).
#![no_main]

use core_base::CancelToken;
use core_diff::{AlignConfig, AlignKind, AlignOp, AlignRange, AlignScratch, VecSource, align_with_scratch};
use core_text::rules::LineRules;
use libfuzzer_sys::fuzz_target;

const POOL: [&str; 16] = [
    "alpha", "beta", "gamma", "", "   ", "  alpha", "alpha  ", "// note", "int x = 1;", "int x = 2;",
    "return v;", "return w;", "one two three", "one two thre", "}", "end;",
];

fuzz_target!(|data: &[u8]| {
    let [skew_byte, opt_byte, rest @ ..] = data else { return };
    let skew = [1u32, 2, 3, 7][(*skew_byte & 3) as usize];
    let cfg = AlignConfig {
        kind: if opt_byte & 1 == 0 { AlignKind::Standard } else { AlignKind::None },
        skew_tolerance: skew,
        use_closeness: Some(opt_byte & 2 != 0),
        closeness_same_only: opt_byte & 4 != 0,
        never_align_diffs: false,
        orphans_always_sig: opt_byte & 8 != 0,
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut on_left = true;
    for &b in rest.iter().take(400) {
        if b == 0xFF {
            on_left = false;
        } else {
            (if on_left { &mut left } else { &mut right }).push(POOL[(b & 15) as usize].to_string());
        }
    }
    let src = VecSource::new(&left, &right, &LineRules::new());
    let mut ops: Vec<AlignOp> = Vec::new();
    let mut scratch = AlignScratch::new();
    align_with_scratch(&src, &cfg, AlignRange::full(), &CancelToken::new(), &mut |o| ops.push(o), &mut scratch)
        .expect("a valid configuration aligns");

    let (mut s, mut t) = (0u32, 0u32);
    for op in &ops {
        assert!(op.amount >= 1);
        if let Some(x) = op.src_opt() {
            assert_eq!(x, s);
            s += op.amount;
        }
        if let Some(x) = op.tgt_opt() {
            assert_eq!(x, t);
            t += op.amount;
        }
    }
    assert_eq!((s as usize, t as usize), (left.len(), right.len()));
});
