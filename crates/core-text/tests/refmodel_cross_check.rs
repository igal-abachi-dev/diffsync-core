//! Cross-check of `LineRules::normalize` and `LineMeta::compute` against the independent
//! Python reference model (backlog E1-S4, E1-S6.1). The expected values below were produced
//! by `.claude/skills/golden-case/scripts/refmodel.py` (functions `normalize`, `line_hash`),
//! not by this crate. To refresh them, run the snippet in the commit message of the change
//! that added this file, or use `refmodel.py hash` for single values.

use core_text::meta::LineMeta;
use core_text::rules::{LineRules, Scratch, WhitespaceMode};

struct Case {
    line: &'static str,
    ws: WhitespaceMode,
    ignore_case: bool,
    unimportant: &'static [&'static str],
    comparable: &'static str,
    hash: u32,
    weight: i32,
    sig: bool,
}

const fn case(
    line: &'static str,
    ws: WhitespaceMode,
    ignore_case: bool,
    unimportant: &'static [&'static str],
    comparable: &'static str,
    hash: u32,
    weight: i32,
    sig: bool,
) -> Case {
    Case {
        line,
        ws,
        ignore_case,
        unimportant,
        comparable,
        hash,
        weight,
        sig,
    }
}

use WhitespaceMode::{CompareAll, IgnoreAll, IgnoreChanges, IgnoreLeadingTrailing as Trim};

const CASES: &[Case] = &[
    case(
        "  a   b  ",
        CompareAll,
        false,
        &[],
        "  a   b  ",
        501_154_323,
        70,
        true,
    ),
    case(
        "  a   b  ",
        Trim,
        false,
        &[],
        "a   b",
        728_971_708,
        70,
        true,
    ),
    case(
        "  a   b  ",
        IgnoreChanges,
        false,
        &[],
        "a b",
        2_053_907_640,
        70,
        true,
    ),
    case(
        "  a   b  ",
        IgnoreAll,
        false,
        &[],
        "ab",
        183_155_104,
        70,
        true,
    ),
    case(
        "\u{A0}x\u{3000}y\t",
        IgnoreChanges,
        false,
        &[],
        "x y",
        4_152_605_809,
        70,
        true,
    ),
    case(
        "Hello World",
        CompareAll,
        true,
        &[],
        "hello world",
        607_555_599,
        70,
        true,
    ),
    case(
        "שלום  עולם",
        IgnoreAll,
        false,
        &[],
        "שלוםעולם",
        1_778_207_109,
        70,
        true,
    ),
    case(
        "  int x; // note",
        Trim,
        false,
        &["//.*$"],
        "int x;",
        1_131_456_221,
        70,
        true,
    ),
    case(
        "   // only",
        Trim,
        false,
        &["//.*$"],
        "",
        2_862_557_818,
        31,
        false,
    ),
    case("", Trim, false, &[], "", 4_294_967_295, 2, false),
    case("\t \t", Trim, false, &[], "", 415_374_307, 31, false),
    case(
        "MiXeD  Case\tTabs",
        IgnoreChanges,
        true,
        &[],
        "mixed case tabs",
        2_838_835_855,
        70,
        true,
    ),
];

#[test]
fn matches_the_reference_model() {
    let mut scratch = Scratch::new();
    for c in CASES {
        let rules = LineRules::new()
            .with_whitespace(c.ws)
            .with_ignore_case(c.ignore_case)
            .with_unimportant(c.unimportant.iter().copied())
            .unwrap();
        let comparable = rules.normalize(c.line, &mut scratch).to_owned();
        assert_eq!(
            comparable, c.comparable,
            "comparable text of {:?} ({:?})",
            c.line, c.ws
        );
        let m = LineMeta::compute(c.line, &rules, &mut scratch, None);
        assert_eq!(m.hash, c.hash, "hash of {:?} ({:?})", c.line, c.ws);
        assert_eq!(m.weight, c.weight, "weight of {:?}", c.line);
        assert_eq!(m.is_sig(), c.sig, "significance of {:?}", c.line);
        assert_eq!(
            m.cmp_len() as usize,
            c.comparable.len(),
            "cmp_len of {:?}",
            c.line
        );
    }
}
