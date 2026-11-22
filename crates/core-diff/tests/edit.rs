//! Editing and copying between the sides (backlog E11.5-S14, decision D62).

use core_diff::edit::{EditDoc, EditLine, Editor, EolKind};
use core_diff::options::EolName;
use core_diff::{DocSpec, Side, Status, TextCompare, TextOptions};
use core_text::{DecodeOptions, Encoding, LineEnding, TextEncoding};

fn compare(l: &str, r: &str) -> std::sync::Arc<TextCompare> {
    let tc = TextCompare::start(
        DocSpec::Text(l.into()),
        DocSpec::Text(r.into()),
        TextOptions::default(),
        None,
    )
    .unwrap();
    tc.wait();
    assert_eq!(tc.status(), Status::Done);
    tc
}

fn texts(e: &Editor) -> (String, String) {
    (e.doc(Side::Left).text(), e.doc(Side::Right).text())
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn random_text(rng: &mut Rng, lines: usize) -> String {
    let pool = ["alpha", "beta", "gamma", "", "delta", "x = 1", "}", "{"];
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(pool[rng.below(pool.len())]);
        s.push_str(&format!(" {}", i % 5));
        s.push_str(["\n", "\r\n"][rng.below(2)]);
    }
    if rng.below(3) == 0 && s.ends_with('\n') {
        s = s.trim_end_matches(['\r', '\n']).to_string();
    }
    s
}

fn mutate(rng: &mut Rng, t: &str) -> String {
    let mut lines: Vec<String> = t.split_inclusive('\n').map(str::to_string).collect();
    for _ in 0..=rng.below(5) {
        match rng.below(3) {
            0 if !lines.is_empty() => {
                lines.remove(rng.below(lines.len()));
            }
            1 => {
                let at = rng.below(lines.len() + 1);
                lines.insert(at, format!("new {}\n", rng.below(4)));
            }
            _ if !lines.is_empty() => {
                let at = rng.below(lines.len());
                let body = lines[at].trim_end_matches(['\r', '\n']).to_string();
                let eol = lines[at][body.len()..].to_string();
                lines[at] = format!("{body} changed{eol}");
            }
            _ => {}
        }
    }
    lines.concat()
}

/// The lines without their endings.
fn contents(t: &str) -> Vec<String> {
    t.split_inclusive('\n')
        .map(|l| l.trim_end_matches(['\r', '\n']).to_string())
        .collect()
}

#[test]
fn a_section_is_copied_across_and_undo_puts_it_back() {
    let tc = compare("a\nb\nc\nd\ne\n", "a\nB\nc\nd\nE\nF\n");
    let mut ed = Editor::begin(tc.clone()).unwrap();
    let o = ed.copy_section(Side::Left, 0).unwrap();
    assert!(o.changed);
    assert_eq!(texts(&ed).1, "a\nb\nc\nd\nE\nF\n");
    // the compare shows the new text
    tc.wait();
    assert_eq!(tc.stats().sections, 1);
    ed.copy_section(Side::Left, 0).unwrap();
    assert_eq!(texts(&ed).1, "a\nb\nc\nd\ne\n");
    tc.wait();
    assert_eq!(tc.stats().sections, 0);
    assert!(ed.undo(Side::Right).unwrap());
    assert_eq!(texts(&ed).1, "a\nb\nc\nd\nE\nF\n");
    assert!(ed.undo(Side::Right).unwrap());
    assert_eq!(texts(&ed).1, "a\nB\nc\nd\nE\nF\n");
    assert!(!ed.undo(Side::Right).unwrap());
    assert!(!ed.doc(Side::Right).is_dirty());
    assert!(ed.redo(Side::Right).unwrap());
    assert!(ed.doc(Side::Right).is_dirty());
}

#[test]
fn a_gap_row_copies_as_a_delete_and_an_orphan_as_an_insert() {
    let tc = compare("a\nx\nb\n", "a\nb\n");
    let mut ed = Editor::begin(tc.clone()).unwrap();
    // row 1 is `x` on the left only: copying right-to-left removes it from the left
    let mut rows = [core_diff::DiffRow {
        left: 0,
        right: 0,
        kind: core_diff::RowKind::Same,
    }; 3];
    tc.rows(0, &mut rows);
    assert_eq!(rows[1].right, u32::MAX);
    ed.copy_rows(Side::Right, 1, 1).unwrap();
    assert_eq!(texts(&ed).0, "a\nb\n");
    // and left to right inserts it again
    let tc2 = compare("a\nx\nb\n", "a\nb\n");
    let mut ed2 = Editor::begin(tc2).unwrap();
    ed2.copy_rows(Side::Left, 1, 1).unwrap();
    assert_eq!(texts(&ed2).1, "a\nx\nb\n");
}

#[test]
fn copying_all_sections_makes_the_sides_equal_for_random_pairs() {
    let mut rng = Rng(0x00C0_FFEE_0ACE_1234);
    for case in 0..120 {
        let n = 2 + rng.below(40);
        let a = random_text(&mut rng, n);
        let b = mutate(&mut rng, &a);
        let tc = compare(&a, &b);
        let mut ed = Editor::begin(tc.clone()).unwrap();
        let dir = if rng.below(2) == 0 {
            Side::Left
        } else {
            Side::Right
        };
        let mut guard = 0;
        while tc.stats().sections > 0 {
            ed.copy_section(dir, 0).unwrap();
            tc.wait();
            guard += 1;
            assert!(guard < 200, "case {case}: copying does not converge");
        }
        let (l, r) = texts(&ed);
        assert_eq!(contents(&l), contents(&r), "case {case}\n{a:?}\n{b:?}");
        // every copy is one undo step, so undoing them all returns the original text
        let to = if dir == Side::Left {
            Side::Right
        } else {
            Side::Left
        };
        while ed.undo(to).unwrap() {}
        let original = if to == Side::Right { &b } else { &a };
        assert_eq!(&ed.doc(to).text(), original, "case {case} undo");
    }
}

#[test]
fn the_open_last_line_and_line_endings_follow_the_target() {
    // the right file uses CRLF; copied lines take its endings
    let tc = compare("a\nX\nc\n", "a\r\nb\r\nc\r\n");
    let mut ed = Editor::begin(tc).unwrap();
    ed.copy_section(Side::Left, 0).unwrap();
    assert_eq!(texts(&ed).1, "a\r\nX\r\nc\r\n");
    // a last line without a newline is copied as the last line without a newline
    let tc = compare("a\nb", "a\nq\n");
    let mut ed = Editor::begin(tc).unwrap();
    ed.copy_section(Side::Left, 0).unwrap();
    assert_eq!(texts(&ed).1, "a\nb");
    // lines added after an open last line close it
    let tc = compare("a\nb\nc\n", "a");
    let mut ed = Editor::begin(tc).unwrap();
    ed.copy_all(Side::Left, false).unwrap();
    assert_eq!(texts(&ed).1, "a\nb\nc\n");
    let mut d = EditDoc::from_text("a");
    d.replace_text(1, 0, "b").unwrap();
    assert_eq!(d.text(), "a\nb");
    // when line endings are compared, the source endings are copied
    let opts = TextOptions {
        eol: EolName::Strict,
        ..TextOptions::default()
    };
    let tc = TextCompare::start(
        DocSpec::Text("a\r\nX\r\n".into()),
        DocSpec::Text("a\nb\n".into()),
        opts,
        None,
    )
    .unwrap();
    tc.wait();
    let mut ed = Editor::begin(tc).unwrap();
    ed.copy_section(Side::Left, 0).unwrap();
    assert_eq!(texts(&ed).1, "a\r\nX\r\n");
}

#[test]
fn line_editing_and_conversion_are_undoable() {
    let mut d = EditDoc::from_text("one\ntwo\nthree\n");
    d.replace_text(1, 1, "2\n2b").unwrap();
    assert_eq!(d.text(), "one\n2\n2b\nthree\n");
    assert!(d.splice(9, 0, vec![]).is_err());
    assert!(
        d.splice(
            0,
            0,
            vec![EditLine {
                text: "a\nb".into(),
                eol: LineEnding::Lf
            }]
        )
        .is_err()
    );
    assert!(d.convert_eol(EolKind::CrLf).unwrap());
    assert_eq!(d.text(), "one\r\n2\r\n2b\r\nthree\r\n");
    assert!(!d.convert_eol(EolKind::CrLf).unwrap());
    assert_eq!(d.default_eol(), EolKind::CrLf);
    assert!(d.undo() && d.undo());
    assert_eq!(d.text(), "one\ntwo\nthree\n");
    assert!(!d.can_undo() && d.can_redo() && !d.is_dirty());
    assert!(d.redo() && d.redo());
    assert_eq!(d.text(), "one\r\n2\r\n2b\r\nthree\r\n");
    // mixed endings are reported, never guessed away
    let m = EditDoc::from_text("a\r\nb\nc\r");
    assert!(m.is_mixed());
    assert_eq!(m.state().ending_counts, [0, 1, 1, 1]);
}

#[test]
fn encodings_are_converted_never_lossily() {
    let mut d = EditDoc::from_text("café\n日本\n");
    // a Western code page cannot hold the second line
    let w = TextEncoding::parse("windows-1252").unwrap();
    let e = d.convert_encoding(w).unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("column 1"), "{e}");
    assert_eq!(d.format().label(), "utf-8");
    // UTF-8 with BOM, UTF-16 both ways: the bytes change, the text does not
    for (label, head) in [
        ("utf-8-bom", vec![0xEF, 0xBB, 0xBF]),
        ("utf-16le", vec![0xFF, 0xFE]),
        ("utf-16be", vec![0xFE, 0xFF]),
        ("utf-16le-nobom", vec![]),
    ] {
        let t = TextEncoding::parse(label).unwrap();
        d.convert_encoding(t).unwrap();
        let bytes = d.to_bytes().unwrap();
        if !head.is_empty() {
            assert!(bytes.starts_with(&head), "{label}");
        }
        let back = EditDoc::from_bytes(bytes, &DecodeOptions::default(), Some(t)).unwrap();
        assert_eq!(back.text(), "café\n日本\n", "{label}");
        assert_eq!(back.format(), t, "{label}");
    }
    // a Hebrew file in windows-1255 round-trips byte for byte
    let hebrew = TextEncoding::parse("windows-1255").unwrap();
    let raw = vec![0xE0, 0xE1, 0xE2, b'\r', b'\n'];
    let doc = EditDoc::from_bytes(raw.clone(), &DecodeOptions::default(), Some(hebrew)).unwrap();
    assert_eq!(doc.to_bytes().unwrap(), raw);
    // "Encode in": the same bytes read as another encoding
    let as_latin = EditDoc::from_bytes(
        raw,
        &DecodeOptions::default(),
        TextEncoding::parse("windows-1252"),
    )
    .unwrap();
    assert_ne!(as_latin.text(), doc.text());
    // malformed bytes: readable, not writable until converted
    let mut bad = EditDoc::from_bytes(
        vec![b'a', 0xFF, 0xFE, b'z'],
        &DecodeOptions::default(),
        Some(TextEncoding::new(Encoding::Utf8, false)),
    )
    .unwrap();
    assert!(bad.is_lossy());
    assert!(bad.to_bytes().is_err());
    assert!(bad.splice(0, 0, vec![]).is_err());
    assert!(
        bad.convert_encoding(TextEncoding::new(Encoding::Utf8, false))
            .unwrap()
    );
    assert!(bad.to_bytes().is_ok());
}

#[test]
fn a_binary_side_is_not_edited() {
    let tc = TextCompare::start(
        DocSpec::Bytes(vec![0, 1, 0, 2, 0, 0]),
        DocSpec::Text("x".into()),
        TextOptions::default(),
        None,
    )
    .unwrap();
    tc.wait();
    assert!(Editor::begin(tc).is_err());
}

// ---------------------------------------------------------------------------------------------
// range edits, as a piece-tree text buffer reports them

use core_diff::edit::RangeEdit;

fn re(sl: usize, sc: usize, el: usize, ec: usize, text: &str) -> RangeEdit {
    RangeEdit {
        start_line: sl,
        start_column: sc,
        end_line: el,
        end_column: ec,
        text: text.into(),
    }
}

#[test]
fn a_range_edit_changes_only_the_lines_it_touches() {
    let mut d = EditDoc::from_text("one\r\ntwo\r\nthree\r\n");
    // typing in the middle of a line
    d.replace_range(&re(1, 1, 1, 1, "XY")).unwrap();
    assert_eq!(d.text(), "one\r\ntXYwo\r\nthree\r\n");
    // a line break typed in: it takes the document's ending
    d.replace_range(&re(1, 3, 1, 3, "\n")).unwrap();
    assert_eq!(d.text(), "one\r\ntXY\r\nwo\r\nthree\r\n");
    // a selection across lines replaced by text
    d.replace_range(&re(0, 2, 2, 1, "!")).unwrap();
    assert_eq!(d.text(), "on!o\r\nthree\r\n");
    // deleting the end of a line joins it with the next
    d.replace_range(&re(0, 4, 1, 0, "")).unwrap();
    assert_eq!(d.text(), "on!othree\r\n");
    // the end of the text: the last line keeps its ending
    d.replace_range(&re(0, 9, 0, 9, "\nend")).unwrap();
    assert_eq!(d.text(), "on!othree\r\nend\r\n");
    // undo takes back one edit at a time
    assert!(d.undo());
    assert_eq!(d.text(), "on!othree\r\n");
    // errors change nothing
    for bad in [
        re(5, 0, 5, 0, "x"),
        re(0, 99, 0, 99, "x"),
        re(0, 3, 0, 1, "x"),
    ] {
        assert!(d.replace_range(&bad).is_err());
    }
    assert_eq!(d.text(), "on!othree\r\n");
}

#[test]
fn columns_count_utf16_units_and_never_split_a_pair() {
    let mut d = EditDoc::from_text("a\u{1F600}b\n");
    // the emoji is two units: columns 1..3
    assert!(
        d.replace_range(&re(0, 2, 0, 2, "x")).is_err(),
        "inside a surrogate pair"
    );
    d.replace_range(&re(0, 1, 0, 3, "-")).unwrap();
    assert_eq!(d.text(), "a-b\n");
    // an empty document accepts an insertion at the origin
    let mut e = EditDoc::from_text("");
    e.replace_range(&re(0, 0, 0, 0, "hello\nworld")).unwrap();
    assert_eq!(e.text(), "hello\nworld");
    assert!(
        EditDoc::from_text("")
            .replace_range(&re(0, 1, 0, 1, "x"))
            .is_err()
    );
}

#[test]
fn a_batch_applies_against_the_current_text_as_one_undo_step() {
    let mut d = EditDoc::from_text("aaa\nbbb\nccc\n");
    d.apply_edits(&[
        re(2, 0, 2, 3, "C"),
        re(0, 0, 0, 1, "A"),
        re(1, 3, 1, 3, "!"),
    ])
    .unwrap();
    assert_eq!(d.text(), "Aaa\nbbb!\nC\n");
    assert!(d.undo());
    assert_eq!(d.text(), "aaa\nbbb\nccc\n");
    assert!(!d.can_undo());
    // overlapping edits are refused and nothing is applied
    assert!(
        d.apply_edits(&[re(0, 0, 0, 2, "x"), re(0, 1, 0, 3, "y")])
            .is_err()
    );
    assert_eq!(d.text(), "aaa\nbbb\nccc\n");
    d.set_text("brand\nnew").unwrap();
    assert_eq!(d.text(), "brand\nnew");
    assert!(d.undo());
    assert_eq!(d.text(), "aaa\nbbb\nccc\n");
}

/// The core document and a host buffer (here a `String` with UTF-16 offsets, the model of a piece
/// tree) stay equal under random range edits.
#[test]
fn the_document_follows_a_host_buffer_through_random_edits() {
    fn utf16_len(s: &str) -> usize {
        s.chars().map(char::len_utf16).sum()
    }
    let mut rng = Rng(0xFEED_FACE_1234_5678);
    let alphabet = ["a", "b", " ", "\u{e9}", "\u{1F600}", "\n", "xyz"];
    for case in 0..200 {
        let mut host = String::from("first line\nsecond\n\nfourth \u{1F600} line\nlast");
        let mut doc = EditDoc::from_text(&host);
        for step in 0..30 {
            // choose two positions in the host text
            let lines: Vec<&str> = host.split('\n').collect();
            let pick = |rng: &mut Rng| {
                let l = rng.below(lines.len());
                let chars: Vec<char> = lines[l].chars().collect();
                let k = rng.below(chars.len() + 1);
                let col: usize = chars[..k].iter().map(|c| c.len_utf16()).sum();
                (l, col, k)
            };
            let (mut a, mut b) = (pick(&mut rng), pick(&mut rng));
            if (a.0, a.1) > (b.0, b.1) {
                std::mem::swap(&mut a, &mut b);
            }
            let text: String = (0..rng.below(4))
                .map(|_| alphabet[rng.below(alphabet.len())])
                .collect();
            // the same edit on the host string
            let byte = |l: usize, k: usize| -> usize {
                let before: usize = lines[..l].iter().map(|x| x.len() + 1).sum();
                before
                    + lines[l]
                        .char_indices()
                        .nth(k)
                        .map_or(lines[l].len(), |(i, _)| i)
            };
            let (sb, eb) = (byte(a.0, a.2), byte(b.0, b.2));
            host.replace_range(sb..eb, &text);
            doc.replace_range(&re(a.0, a.1, b.0, b.1, &text))
                .unwrap_or_else(|e| panic!("case {case} step {step}: {e}"));
            assert_eq!(doc.text(), host, "case {case} step {step}");
            let _ = utf16_len(&host);
        }
        // and undo walks the same way back to the start
        while doc.undo() {}
        assert_eq!(
            doc.text(),
            "first line\nsecond\n\nfourth \u{1F600} line\nlast",
            "case {case}"
        );
    }
}

#[test]
fn edits_can_wait_for_a_refresh() {
    let tc = compare("a\nb\n", "a\nb\n");
    let mut ed = Editor::begin(tc.clone()).unwrap();
    let g0 = tc.generation();
    let r = RangeEdit {
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 1,
        text: "!".into(),
    };
    // typing: the compare is not restarted per keystroke
    ed.replace_range(Side::Right, &r, false).unwrap();
    ed.replace_range(
        Side::Right,
        &RangeEdit {
            start_column: 2,
            end_column: 2,
            ..r.clone()
        },
        false,
    )
    .unwrap();
    assert!(ed.is_stale());
    assert_eq!(tc.generation(), g0);
    assert_eq!(texts(&ed).1, "a\nb!!\n");
    // a copy needs the rows, so it brings the compare up to date first
    ed.copy_section(Side::Left, 0).unwrap();
    assert!(!ed.is_stale());
    assert_eq!(texts(&ed).1, "a\nb\n");
}
