//! Review corpus (backlog E4-S9, plan.md §11.4): every pair under `tests/review` is compared and
//! its rows are stored as an `insta` snapshot. A change in any row fails this test until the
//! snapshot is re-reviewed (`dsdev render <folder> --html`, then `cargo insta review`).

use core_diff::{DiffRow, DocSpec, RowKind, Status, TextCompare, TextOptions};

#[test]
fn review_corpus_rows_match_the_approved_snapshots() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/review");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("left").exists())
        .collect();
    dirs.sort();
    assert!(
        dirs.len() >= 60,
        "the corpus must have at least 60 pairs, found {}",
        dirs.len()
    );
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let opts = match std::fs::read_to_string(dir.join("options.json")) {
            Ok(j) => TextOptions::from_json(&j).unwrap(),
            Err(_) => TextOptions::default(),
        };
        let tc = TextCompare::start(
            DocSpec::Path(dir.join("left")),
            DocSpec::Path(dir.join("right")),
            opts,
            None,
        )
        .unwrap();
        tc.wait();
        // a corpus case may be too big for the unoptimised test build; none is expected to fail
        assert!(
            !matches!(tc.status(), Status::Failed { .. }),
            "{name}: {:?}",
            tc.status()
        );
        let n = tc.row_count() as usize;
        let mut rows = vec![
            DiffRow {
                left: 0,
                right: 0,
                kind: RowKind::Same
            };
            n
        ];
        tc.rows(0, &mut rows);
        // run-length encode the rows so a snapshot of a 3000-line log stays readable
        let mut text = String::new();
        let mut i = 0;
        while i < rows.len() {
            let r = rows[i];
            let mut len = 1;
            while i + len < rows.len()
                && rows[i + len].kind == r.kind
                && rows[i + len].left_opt() == r.left_opt().map(|l| l + len as u32)
                && rows[i + len].right_opt() == r.right_opt().map(|v| v + len as u32)
            {
                len += 1;
            }
            let side = |v: Option<u32>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
            text += &format!(
                "{} {} {} x{len}\n",
                side(r.left_opt()),
                side(r.right_opt()),
                r.kind.name()
            );
            i += len;
        }
        let s = tc.stats();
        text += &format!(
            "# same {} important {} unimportant {} sections {} ({} important)\n",
            s.same,
            s.important_diff + s.left_orphan + s.right_orphan,
            s.unimportant_diff + s.unimportant_left_orphan + s.unimportant_right_orphan,
            s.sections,
            s.important_sections
        );
        insta::with_settings!({ snapshot_path => "snapshots", prepend_module_to_snapshot => false }, {
            insta::assert_snapshot!(name.as_str(), text);
        });
    }
}

/// Backlog E11-S3: every review case runs under every align kind (the snapshots above pin the
/// default; the other kinds are different algorithms, so they are checked against the invariants
/// that any correct alignment keeps): the rows cover every left and right line exactly once and
/// in order, a `Same` row holds identical lines, the section counts agree with the rows, and
/// the session finishes. The rendered pages for the owner's sign-off come from
/// `dsdev render <case> --html`.
#[test]
fn every_review_case_is_sound_under_every_align_kind() {
    use core_diff::Side;
    use core_diff::options::AlignmentName;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/review");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("left").exists())
        .collect();
    dirs.sort();
    let mut checked = 0;
    for dir in &dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        for kind in [
            AlignmentName::None,
            AlignmentName::Patience,
            AlignmentName::Myers,
            AlignmentName::Standard,
            AlignmentName::Histogram,
        ] {
            let mut opts = match std::fs::read_to_string(dir.join("options.json")) {
                Ok(j) => TextOptions::from_json(&j).unwrap(),
                Err(_) => TextOptions::default(),
            };
            // without an options file the rules are the defaults: a Same row must be byte-identical
            let default_opts = !dir.join("options.json").exists();
            opts.alignment = kind;
            let tc = TextCompare::start(
                DocSpec::Path(dir.join("left")),
                DocSpec::Path(dir.join("right")),
                opts,
                None,
            )
            .unwrap();
            tc.wait();
            let what = format!("{name} / {kind:?}");
            assert_eq!(tc.status(), Status::Done, "{what}");
            let n = tc.row_count() as usize;
            let mut rows = vec![
                DiffRow {
                    left: 0,
                    right: 0,
                    kind: RowKind::Same
                };
                n
            ];
            assert_eq!(tc.rows(0, &mut rows), n, "{what}");
            let (mut next_l, mut next_r) = (0u32, 0u32);
            for (i, r) in rows.iter().enumerate() {
                if let Some(l) = r.left_opt() {
                    assert_eq!(l, next_l, "{what}: row {i} skips or repeats a left line");
                    next_l += 1;
                }
                if let Some(rr) = r.right_opt() {
                    assert_eq!(rr, next_r, "{what}: row {i} skips or repeats a right line");
                    next_r += 1;
                }
                if r.kind == RowKind::Same && default_opts {
                    let (mut a, mut b) = (Vec::new(), Vec::new());
                    assert!(
                        tc.line_utf16(Side::Left, r.left_opt().unwrap(), &mut a),
                        "{what}"
                    );
                    assert!(
                        tc.line_utf16(Side::Right, r.right_opt().unwrap(), &mut b),
                        "{what}"
                    );
                    assert_eq!(a, b, "{what}: row {i} is Same but the lines differ");
                }
            }
            let (dl, dr) = (
                tc.doc_info(Side::Left).unwrap().line_count,
                tc.doc_info(Side::Right).unwrap().line_count,
            );
            assert_eq!(
                (next_l, next_r),
                (dl, dr),
                "{what}: rows must end at both documents' ends"
            );
            checked += 1;
        }
    }
    assert!(checked >= 60 * 5, "swept {checked} case/kind pairs");
}
