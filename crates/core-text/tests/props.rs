//! Property tests for `core-text` (backlog E1-S2.3, E1-S4.2, E1-S6.3).

use core_text::fold::fold_into;
use core_text::meta::{LineMeta, MetaCursor, compute_all_parallel};
use core_text::rules::{LineRules, Scratch, WhitespaceMode};
use core_text::{TextDocument, index_lines};
use proptest::prelude::*;

/// Text over a small alphabet that is heavy in line-ending characters, so every ending mix
/// (including `\r\r\n`, `\n\r`, a trailing `\r`) shows up often.
fn endings_heavy() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            Just(b'a'),
            Just(b'b'),
            Just(b'\r'),
            Just(b'\n'),
            Just(b' '),
            Just(0xC3u8), // expanded to a valid two-byte character below
        ],
        0..200,
    )
    .prop_map(|v| {
        let mut out = Vec::with_capacity(v.len() * 2);
        for b in v {
            out.push(b);
            if b == 0xC3 {
                out.push(0xA9);
            }
        }
        out
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, ..ProptestConfig::default() })]

    /// Joining every line with its own ending gives back the original bytes, spans are
    /// contiguous and ordered, and only the last line may lack an ending.
    #[test]
    fn line_index_join_equals_the_original(text in endings_heavy()) {
        let idx = index_lines(&text).unwrap();
        let mut rebuilt = Vec::with_capacity(text.len());
        let mut expect_start = 0u64;
        for (i, s) in idx.spans.iter().enumerate() {
            prop_assert_eq!(s.start, expect_start, "span {} is not contiguous", i);
            let a = s.start as usize;
            rebuilt.extend_from_slice(&text[a..a + s.len as usize]);
            rebuilt.extend_from_slice(s.ending.as_str().as_bytes());
            expect_start = s.next_start();
            if s.ending.is_empty() {
                prop_assert_eq!(i, idx.spans.len() - 1, "only the last line may lack an ending");
                prop_assert!(s.len > 0, "an endingless last line is never empty");
            }
        }
        prop_assert_eq!(&rebuilt, &text);
        let total: u64 = idx.ending_counts.iter().sum();
        prop_assert_eq!(total as usize, idx.len());
    }
}

proptest! {
    /// Simple case folding keeps the length in code points (decision D5) for any text, and
    /// folding twice changes nothing more.
    #[test]
    fn folding_preserves_code_point_count(s in "\\PC{0,60}") {
        let mut once = String::new();
        fold_into(&s, &mut once);
        prop_assert_eq!(once.chars().count(), s.chars().count());
        let mut twice = String::new();
        fold_into(&once, &mut twice);
        prop_assert_eq!(&twice, &once);
    }

    /// Normalizing a normalized line changes nothing.
    #[test]
    fn normalize_is_idempotent(
        line in "[ \\t\\u{A0}a-cA-C\\u{5D0}-\\u{5D3}/#]{0,40}",
        mode in prop_oneof![
            Just(WhitespaceMode::CompareAll),
            Just(WhitespaceMode::IgnoreLeadingTrailing),
            Just(WhitespaceMode::IgnoreChanges),
            Just(WhitespaceMode::IgnoreAll),
        ],
        case in any::<bool>(),
    ) {
        let rules = LineRules::new()
            .with_whitespace(mode)
            .with_ignore_case(case)
            .with_unimportant(["#.*$"])
            .unwrap();
        let mut sc = Scratch::new();
        let once = rules.normalize(&line, &mut sc).to_owned();
        let twice = rules.normalize(&once, &mut sc).to_owned();
        prop_assert_eq!(&once, &twice);
    }

    /// Lazy chunked computation and parallel precompute give identical metadata.
    #[test]
    fn parallel_and_lazy_metadata_agree(
        lines in proptest::collection::vec("[ a-cA-C\\u{5D0}-\\u{5D2}/#]{0,12}", 0..400),
        case in any::<bool>(),
    ) {
        let text = lines.join("\n");
        let doc = TextDocument::from_text(&text).unwrap();
        let rules = LineRules::new().with_ignore_case(case).with_unimportant(["#.*$"]).unwrap();
        let par = compute_all_parallel(&doc, &rules, None);
        let mut cur = MetaCursor::new(&doc, &rules, None);
        let mut sc = Scratch::new();
        prop_assert_eq!(par.len(), doc.line_count());
        // visit in a scrambled order so the cursor chunking is exercised
        let n = doc.line_count();
        for k in 0..n {
            let i = (k * 7919) % n.max(1);
            let direct = LineMeta::compute(doc.line(i), &rules, &mut sc, None);
            prop_assert_eq!(cur.get(i), direct);
            prop_assert_eq!(par[i], direct);
        }
    }
}
