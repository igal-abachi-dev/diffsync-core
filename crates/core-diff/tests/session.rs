//! The text compare session (backlog E4-S1..S4, S8): lifecycle, progressive rows, events,
//! panic capture, document info, binary documents, and the golden rows through the real
//! classifier.

#[path = "../../../tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use core_diff::session::{Dir, EventSink, event};
use core_diff::{DiffRow, DocSpec, RowKind, Side, Status, TextCompare, TextOptions};
use core_text::LoadOptions;

fn text(lines: &[&str]) -> DocSpec {
    DocSpec::Text(lines.iter().map(|l| format!("{l}\n")).collect())
}

fn wait_done(tc: &TextCompare) -> Status {
    tc.wait();
    tc.status()
}

fn all_rows(tc: &TextCompare) -> Vec<DiffRow> {
    let n = tc.row_count() as usize;
    let mut out = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        n
    ];
    assert_eq!(tc.rows(0, &mut out), n);
    out
}

fn kinds(tc: &TextCompare) -> Vec<RowKind> {
    all_rows(tc).iter().map(|r| r.kind).collect()
}

/// Every golden case through the session gives the expected rows (classification, E4-S2.1).
#[test]
fn golden_cases_through_the_session() {
    let root = common::fixtures_root(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0;
    for dir in common::list_cases(&root).unwrap() {
        let case = common::load_case(&dir).unwrap();
        let opts = case
            .options_json
            .as_deref()
            .map_or_else(|| Ok(TextOptions::default()), TextOptions::from_json)
            .unwrap();
        let tc = TextCompare::start(
            DocSpec::Bytes(case.left.clone()),
            DocSpec::Bytes(case.right.clone()),
            opts,
            None,
        )
        .unwrap();
        assert_eq!(wait_done(&tc), Status::Done, "{}", case.name);
        let got: Vec<common::ExpRow> = all_rows(&tc)
            .iter()
            .map(|r| common::ExpRow {
                left: r.left_opt(),
                right: r.right_opt(),
                kind: r.kind.name().to_string(),
            })
            .collect();
        assert!(
            got == case.rows,
            "{}\n{}",
            case.name,
            common::diff_report("rows", &case.rows, &got)
        );
        checked += 1;
    }
    assert!(checked >= 20);
}

#[test]
fn start_returns_at_once_and_loading_failures_are_reported() {
    let t = Instant::now();
    let tc = TextCompare::start(
        DocSpec::Path("definitely/missing.txt".into()),
        text(&["a"]),
        TextOptions::default(),
        None,
    )
    .unwrap();
    assert!(t.elapsed() < Duration::from_millis(500));
    match wait_done(&tc) {
        Status::Failed { error } => assert!(error.contains("not found"), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bad_options_fail_at_start() {
    let bad = TextOptions {
        unimportant: vec!["(x".into()],
        ..TextOptions::default()
    };
    assert!(TextCompare::start(text(&["a"]), text(&["a"]), bad, None).is_err());
}

#[test]
fn statuses_progress_to_done_with_stats() {
    let tc = TextCompare::start(
        text(&["a", "b", "c"]),
        text(&["a", "x", "c"]),
        TextOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(wait_done(&tc), Status::Done);
    assert_eq!(
        kinds(&tc),
        [RowKind::Same, RowKind::Different, RowKind::Same]
    );
    let s = tc.stats();
    assert_eq!(
        (s.same, s.important_diff, s.sections, s.important_sections),
        (2, 1, 1, 1)
    );
    let mut sec = [core_diff::session::Section {
        first_row: 0,
        row_count: 0,
        important: false,
    }; 2];
    assert_eq!(tc.sections(0, &mut sec), 1);
    assert_eq!(tc.find_diff(0, Dir::Next, true), Some(1));
}

#[test]
fn line_access_is_utf16_without_endings() {
    let tc = TextCompare::start(
        DocSpec::Bytes("שלום\r\n😀 x\n".as_bytes().to_vec()),
        text(&["y"]),
        TextOptions::default(),
        None,
    )
    .unwrap();
    wait_done(&tc);
    let mut buf = vec![7u16];
    assert!(tc.line_utf16(Side::Left, 0, &mut buf));
    assert_eq!(String::from_utf16(&buf).unwrap(), "שלום");
    assert!(tc.line_utf16(Side::Left, 1, &mut buf));
    assert_eq!(
        buf,
        "😀 x".encode_utf16().collect::<Vec<_>>(),
        "surrogate pairs survive"
    );
    assert!(!tc.line_utf16(Side::Left, 2, &mut buf));
    assert!(buf.is_empty());
}

#[test]
fn doc_info_per_encoding() {
    let mut u16le = vec![0xFF, 0xFE];
    u16le.extend("a\r\nb\n".encode_utf16().flat_map(u16::to_le_bytes));
    let tc = TextCompare::start(
        DocSpec::Bytes(u16le),
        DocSpec::Bytes(b"\xEF\xBB\xBFx\ny".to_vec()),
        TextOptions::default(),
        None,
    )
    .unwrap();
    wait_done(&tc);
    let l = tc.doc_info(Side::Left).unwrap();
    assert_eq!(
        (l.encoding.as_str(), l.has_bom, l.line_count, l.is_binary),
        ("UTF-16LE", true, 2, false)
    );
    assert_eq!(l.ending_counts, [0, 1, 1, 0]);
    let r = tc.doc_info(Side::Right).unwrap();
    assert_eq!(
        (r.encoding.as_str(), r.has_bom, r.byte_len),
        ("UTF-8", true, 6)
    );
    assert_eq!(r.ending_counts, [1, 0, 1, 0]);
}

#[test]
fn a_binary_side_gives_status_binary_and_no_rows() {
    let tc = TextCompare::start(
        DocSpec::Bytes(vec![0, 1, 2, 3]),
        text(&["a"]),
        TextOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(wait_done(&tc), Status::Binary);
    assert_eq!(tc.row_count(), 0);
    assert!(tc.doc_info(Side::Left).unwrap().is_binary);
}

/// Backlog E4-S1.4: a panic in the worker becomes `Failed`; the process survives.
#[test]
fn a_worker_panic_becomes_failed() {
    let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let tc = TextCompare::start_with(
        text(&refs),
        text(&refs),
        TextOptions::default(),
        None,
        LoadOptions::default(),
        Some(10),
    )
    .unwrap();
    match wait_done(&tc) {
        Status::Failed { error } => assert!(
            error.contains("panicked") && error.contains("injected"),
            "{error}"
        ),
        other => panic!("{other:?}"),
    }
}

/// Backlog E4-S1.3: cancel stops quickly; recompare re-runs over the same documents.
#[test]
fn cancel_and_recompare() {
    let n = 300_000;
    let a: Vec<String> = (0..n)
        .map(|k| format!("left only line number {k}"))
        .collect();
    let b: Vec<String> = (0..n)
        .map(|k| format!("RIGHT ONLY LINE NUMBER {k}"))
        .collect();
    let tc = TextCompare::start(
        DocSpec::Text(a.join("\n")),
        DocSpec::Text(b.join("\n")),
        TextOptions::default(),
        None,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    tc.cancel();
    tc.wait();
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "cancel took {:?}",
        t.elapsed()
    );
    assert_eq!(tc.status(), Status::Cancelled);

    // recompare over the same documents with a cheaper setting finishes
    tc.recompare(TextOptions::from_json(r#"{"alignment":"none"}"#).unwrap())
        .unwrap();
    assert_eq!(wait_done(&tc), Status::Done);
    assert_eq!(tc.row_count() as usize, n);
}

#[test]
fn recompare_with_new_rules_changes_the_rows() {
    let tc =
        TextCompare::start(text(&["  a"]), text(&["a"]), TextOptions::default(), None).unwrap();
    assert_eq!(wait_done(&tc), Status::Done);
    assert_eq!(kinds(&tc), [RowKind::Unimportant]);
    tc.recompare(TextOptions::from_json(r#"{"whitespace":"all"}"#).unwrap())
        .unwrap();
    assert_eq!(wait_done(&tc), Status::Done);
    assert_eq!(kinds(&tc), [RowKind::Different]);
    assert_eq!(tc.stats().important_diff, 1);
}

/// Backlog E4-S1.2: wake-ups are coalesced to at most about 60 Hz.
#[test]
fn events_are_coalesced() {
    let rows = Arc::new(AtomicU32::new(0));
    let states = Arc::new(AtomicU32::new(0));
    let sink: EventSink = {
        let (rows, states) = (rows.clone(), states.clone());
        Arc::new(move |bits| {
            if bits & event::ROWS != 0 {
                rows.fetch_add(1, Ordering::Relaxed);
            }
            if bits & event::STATE != 0 {
                states.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    let n = 200_000;
    let lines: Vec<String> = (0..n).map(|k| format!("line {k}")).collect();
    let started = Instant::now();
    let tc = TextCompare::start(
        DocSpec::Text(lines.join("\n")),
        DocSpec::Text(lines.join("\n")),
        TextOptions::default(),
        Some(sink),
    )
    .unwrap();
    assert_eq!(wait_done(&tc), Status::Done);
    let secs = started.elapsed().as_secs_f64().max(0.016);
    let got = rows.load(Ordering::Relaxed);
    assert!(got >= 1, "at least the final wake-up arrives");
    assert!(
        f64::from(got) <= secs * 70.0 + 3.0,
        "{got} ROWS events in {secs:.3}s is above 60 Hz"
    );
    assert!(states.load(Ordering::Relaxed) >= 1);
    assert_eq!(tc.row_count() as usize, n);
}

/// Backlog E4-S2.2: readers are not blocked by the aligner for longer than a batch copy.
#[test]
fn readers_make_progress_while_the_aligner_runs() {
    // a difference every 7th line keeps every window busy with a full gap DP, so keep it small
    let n = 20_000;
    let a: Vec<String> = (0..n).map(|k| format!("alpha {k}")).collect();
    let b: Vec<String> = (0..n)
        .map(|k| format!("alpha {}", k + (k % 7 == 0) as usize))
        .collect();
    let tc = TextCompare::start(
        DocSpec::Text(a.join("\n")),
        DocSpec::Text(b.join("\n")),
        TextOptions::default(),
        None,
    )
    .unwrap();
    let mut out = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        100
    ];
    let (mut reads, mut worst) = (0u32, Duration::ZERO);
    while !matches!(
        tc.status(),
        Status::Done | Status::Failed { .. } | Status::Cancelled
    ) {
        let t = Instant::now();
        tc.rows(0, &mut out);
        let _ = tc.stats();
        worst = worst.max(t.elapsed());
        reads += 1;
    }
    tc.wait();
    assert!(reads > 0);
    assert!(worst < Duration::from_millis(50), "a read waited {worst:?}");
}

#[test]
fn intraline_through_the_session() {
    let tc = TextCompare::start(
        text(&["int x = 1;"]),
        text(&["int x = 2;"]),
        TextOptions::default(),
        None,
    )
    .unwrap();
    wait_done(&tc);
    let d = tc.intraline(0).unwrap();
    assert_eq!(
        (d.left[0].start, d.left[0].len, d.left[0].important),
        (8, 1, true)
    );
    assert!(
        Arc::ptr_eq(&d, &tc.intraline(0).unwrap()),
        "second call is a cache hit"
    );
    let same =
        TextCompare::start(text(&["a"]), text(&["a"]), TextOptions::default(), None).unwrap();
    wait_done(&same);
    assert!(
        same.intraline(0).is_none(),
        "identical rows have no intraline"
    );
    assert!(tc.intraline(5).is_none());
}

#[test]
fn overview_and_navigation_on_a_session() {
    let l: Vec<String> = (0..100).map(|k| format!("line {k}")).collect();
    let mut r = l.clone();
    r[50] = "CHANGED".into();
    let tc = TextCompare::start(
        DocSpec::Text(l.join("\n")),
        DocSpec::Text(r.join("\n")),
        TextOptions::default(),
        None,
    )
    .unwrap();
    wait_done(&tc);
    let mut b = [0u8; 10];
    tc.overview(&mut b);
    assert_eq!(b[5], RowKind::Different as u8);
    assert_eq!(b.iter().filter(|&&k| k != 0).count(), 1);
    assert_eq!(tc.find_diff(0, Dir::Next, false), Some(50));
    assert_eq!(tc.find_diff(50, Dir::Prev, false), None);
}

/// Reviewer finding (E11-S6): a `recompare` issued while the documents are still loading used to
/// cancel the load and leave the session `Failed("no documents loaded")` for good. The load now
/// restarts with the new options. A 30 MB file keeps the load window open for some milliseconds,
/// so the call lands inside it; the assertion holds whichever way the race goes.
#[test]
fn recompare_while_loading_restarts_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..600_000)
        .map(|i| format!("line number {i} of the log\n"))
        .collect();
    std::fs::write(dir.path().join("a.txt"), &body).unwrap();
    std::fs::write(
        dir.path().join("b.txt"),
        body.replace("number 7 ", "number  7 "),
    )
    .unwrap();
    for round in 0..12 {
        let tc = TextCompare::start(
            DocSpec::Path(dir.path().join("a.txt")),
            DocSpec::Path(dir.path().join("b.txt")),
            TextOptions::default(),
            None,
        )
        .unwrap();
        let opts = TextOptions::from_json(r#"{"whitespace":"changes"}"#).unwrap();
        tc.recompare(opts.clone()).unwrap();
        assert_eq!(wait_done(&tc), Status::Done, "round {round}");
        assert!(tc.row_count() >= 600_000, "round {round}");
        assert_eq!(
            tc.stats().important_diff,
            0,
            "the new options applied (round {round})"
        );
        // and a later recompare still works
        tc.recompare(TextOptions::default()).unwrap();
        assert_eq!(wait_done(&tc), Status::Done, "round {round}");
    }
}

/// plan §12.2, completely different lines (20k a side here; the bench has the 50k): "no stall of
/// `get_rows`". While the worst
/// case gap DP runs, the read calls (`rows`, `row_count`, `status`, `stats`) keep answering at
/// once. The bound is generous (50 ms) because the test machine may be loaded; a call that waited
/// for the aligner would take seconds.
#[test]
fn reads_never_stall_while_the_worst_case_aligns() {
    let words = [
        "amber", "birch", "cedar", "dune", "ember", "fjord", "glade", "heath",
    ];
    let make = |salt: usize| -> String {
        (0..20_000usize)
            .map(|k| {
                let w =
                    |i: u32| words[(k.wrapping_mul(2_654_435_761).rotate_left(i * 5) ^ salt) % 8];
                format!("{salt}-{k} {} {} {} {}\n", w(1), w(2), w(3), w(4))
            })
            .collect()
    };
    let tc = TextCompare::start(
        DocSpec::Text(make(1)),
        DocSpec::Text(make(2)),
        TextOptions::default(),
        None,
    )
    .unwrap();
    let mut rows = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        100
    ];
    let (mut worst, mut polls) = (Duration::ZERO, 0u32);
    let started = Instant::now();
    loop {
        let t = Instant::now();
        let _ = tc.row_count();
        let _ = tc.rows(0, &mut rows);
        let _ = tc.stats();
        let st = tc.status();
        worst = worst.max(t.elapsed());
        polls += 1;
        if matches!(st, Status::Done | Status::Failed { .. }) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the alignment never finished"
        );
        std::thread::sleep(Duration::from_micros(200));
    }
    assert_eq!(tc.status(), Status::Done);
    assert!(
        polls > 20,
        "the poller must overlap the alignment ({polls} polls)"
    );
    assert!(
        worst < Duration::from_millis(50),
        "a read call stalled for {worst:?}"
    );
}
