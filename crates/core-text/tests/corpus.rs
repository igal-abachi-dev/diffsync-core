//! Encoding corpus (backlog E1-S1.4, E1-S7.2): every encoding x line-ending style in
//! `tests/fixtures/encodings` decodes to the expected lines and endings, and serializing the
//! loaded document gives the original bytes back. Expectations are written from the corpus
//! recipe in `gen.py`, never from the loader's own output.

use std::path::PathBuf;

use core_text::encoding::{DecodeOptions, Encoding};
use core_text::{LineEnding, LoadOptions, TextDocument};
use encoding_rs::{WINDOWS_1252, WINDOWS_1255};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/encodings")
}

const UNICODE_LINE: &str = "שלום עולם 😀";
const HEBREW_LINE: &str = "שלום עולם";
const LATIN_LINE: &str = "café déjà vu";

fn lines_for(non_ascii: &str) -> Vec<String> {
    ["first line", "second", "", non_ascii, "last line"]
        .map(String::from)
        .to_vec()
}

fn endings_for(style: &str) -> Vec<LineEnding> {
    use LineEnding::{Cr, CrLf, Lf, None as NoEnd};
    match style {
        "crlf" => vec![CrLf; 5],
        "lf" => vec![Lf; 5],
        "cr" => vec![Cr; 5],
        "nofinal" => vec![Lf, Lf, Lf, Lf, NoEnd],
        "mixed" => vec![CrLf, Lf, Cr, CrLf, NoEnd],
        other => panic!("unknown style {other}"),
    }
}

/// (file prefix) -> (encoding, has BOM, non-ASCII line, ANSI override)
fn recipe(
    enc: &str,
) -> (
    Encoding,
    bool,
    &'static str,
    Option<&'static encoding_rs::Encoding>,
) {
    match enc {
        "utf8" => (Encoding::Utf8, false, UNICODE_LINE, None),
        "utf8bom" => (Encoding::Utf8Bom, true, UNICODE_LINE, None),
        "utf16le_bom" => (Encoding::Utf16Le, true, UNICODE_LINE, None),
        "utf16be_bom" => (Encoding::Utf16Be, true, UNICODE_LINE, None),
        "utf16le_nobom" => (Encoding::Utf16Le, false, UNICODE_LINE, None),
        "utf16be_nobom" => (Encoding::Utf16Be, false, UNICODE_LINE, None),
        "cp1252" => (
            Encoding::Ansi(WINDOWS_1252),
            false,
            LATIN_LINE,
            Some(WINDOWS_1252),
        ),
        "cp1255" => (
            Encoding::Ansi(WINDOWS_1255),
            false,
            HEBREW_LINE,
            Some(WINDOWS_1255),
        ),
        other => panic!("unknown encoding {other}"),
    }
}

#[test]
fn every_matrix_file_decodes_and_round_trips_byte_for_byte() {
    let mut checked = 0;
    for entry in std::fs::read_dir(dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".txt") || name.starts_with("empty__") || name.starts_with("utf8__only")
        {
            continue;
        }
        let (enc, style) = name.trim_end_matches(".txt").split_once("__").unwrap();
        let (want_enc, want_bom, non_ascii, ansi) = recipe(enc);
        let opts = LoadOptions {
            decode: DecodeOptions { ansi },
            ..LoadOptions::default()
        };
        let doc = TextDocument::load(&path, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(doc.encoding(), want_enc, "{name}: encoding");
        assert_eq!(doc.has_bom(), want_bom, "{name}: BOM");
        assert!(!doc.is_binary(), "{name}: binary");
        assert!(!doc.is_lossy(), "{name}: lossy");

        let got: Vec<&str> = doc.lines().collect();
        assert_eq!(got, lines_for(non_ascii), "{name}: lines");
        let endings: Vec<LineEnding> = (0..doc.line_count()).map(|i| doc.line_ending(i)).collect();
        assert_eq!(endings, endings_for(style), "{name}: endings");

        let original = std::fs::read(&path).unwrap();
        assert_eq!(doc.byte_len(), original.len() as u64, "{name}: byte_len");
        assert_eq!(doc.serialize().unwrap(), original, "{name}: round trip");
        checked += 1;
    }
    assert!(
        checked >= 12,
        "the matrix must cover at least 12 files, found {checked}"
    );
}

#[test]
fn empty_file() {
    let doc = TextDocument::open(&dir().join("empty__none.txt")).unwrap();
    assert_eq!(doc.line_count(), 0);
    assert_eq!(doc.encoding(), Encoding::Utf8);
    assert_eq!(doc.serialize().unwrap(), b"");
}

#[test]
fn only_line_endings_gives_empty_lines() {
    let doc = TextDocument::open(&dir().join("utf8__only_endings.txt")).unwrap();
    assert_eq!(doc.lines().collect::<Vec<_>>(), ["", "", ""]);
    assert_eq!(doc.line_ending(0), LineEnding::CrLf);
    assert_eq!(doc.line_ending(1), LineEnding::CrLf);
    assert_eq!(doc.line_ending(2), LineEnding::Lf);
    assert_eq!(doc.ending_counts(), [0, 2, 1, 0]);
}

#[test]
fn binary_file_is_flagged_and_not_decoded() {
    let doc = TextDocument::open(&dir().join("binary__nul.bin")).unwrap();
    assert!(doc.is_binary());
    assert_eq!(doc.line_count(), 0);
    assert_eq!(doc.byte_len(), 20);
}

/// The same bytes read with two code pages give two different texts: the override is honoured
/// (the system code page of the test machine never matters).
#[test]
fn ansi_override_selects_the_code_page() {
    let path = dir().join("cp1255__crlf.txt");
    let hebrew = LoadOptions {
        decode: DecodeOptions {
            ansi: Some(WINDOWS_1255),
        },
        ..LoadOptions::default()
    };
    let latin = LoadOptions {
        decode: DecodeOptions {
            ansi: Some(WINDOWS_1252),
        },
        ..LoadOptions::default()
    };
    let h = TextDocument::load(&path, &hebrew).unwrap();
    let l = TextDocument::load(&path, &latin).unwrap();
    assert_eq!(h.line(3), HEBREW_LINE);
    assert_ne!(l.line(3), HEBREW_LINE);
    // a document read with the wrong code page can still be written back unchanged
    assert_eq!(l.serialize().unwrap(), std::fs::read(&path).unwrap());
}
