//! The shared fixture loader (`tests/common`) parses the G4 golden case (backlog E0-S6.1).

#[path = "../../../tests/common/mod.rs"]
mod common;

use common::{ExpOp, ExpRow, load_case, parse_ops, parse_rows};

#[test]
fn loads_the_g4_case() {
    let root = common::fixtures_root(env!("CARGO_MANIFEST_DIR"));
    let case = load_case(&root.join("g4_right_only_insert")).expect("G4 loads");
    assert_eq!(case.left, b"a\nb\ncharlie\n");
    assert_eq!(case.right, b"a\nb\nnovember\ncharlie\n");
    assert!(case.options_json.is_none());
    let ops = [
        (Some(0), Some(0), 1),
        (Some(1), Some(1), 1),
        (None, Some(2), 1),
        (Some(2), Some(3), 1),
    ];
    let want: Vec<ExpOp> = ops
        .iter()
        .map(|&(src, tgt, amount)| ExpOp { src, tgt, amount })
        .collect();
    assert_eq!(case.ops, want);
    let kinds = ["Same", "Same", "RightOnly", "Same"];
    assert_eq!(case.rows.len(), 4);
    assert_eq!(
        case.rows[2],
        ExpRow {
            left: None,
            right: Some(2),
            kind: "RightOnly".into()
        }
    );
    for (row, kind) in case.rows.iter().zip(kinds) {
        assert_eq!(row.kind, kind);
    }
}

#[test]
fn parser_skips_comments_and_blank_lines() {
    let ops = parse_ops("# comment\n\n0 0 1\n  - 3 200  \n").unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[1],
        ExpOp {
            src: None,
            tgt: Some(3),
            amount: 200
        }
    );
    assert_eq!(ops[1].to_string(), "- 3 200");
}

#[test]
fn parser_rejects_malformed_input() {
    assert!(parse_ops("0 0").is_err());
    assert!(parse_ops("0 x 1").is_err());
    assert!(parse_ops("- - 1").is_err());
    assert!(parse_ops("0 0 0").is_err());
    assert!(parse_rows("0 0 Samey").is_err());
    assert!(parse_rows("0 0").is_err());
    assert_eq!(
        parse_rows("- 2 RightOnly").unwrap()[0].to_string(),
        "- 2 RightOnly"
    );
}

#[test]
fn diff_report_points_at_the_first_difference() {
    let a = parse_ops("0 0 1\n1 1 1").unwrap();
    let b = parse_ops("0 0 1\n1 1 2").unwrap();
    let report = common::diff_report("ops", &a, &b);
    assert!(
        report.contains("#1: expected `1 1 1`  actual `1 1 2`"),
        "{report}"
    );
}
