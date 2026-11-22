//! Verdict mode: "are these two texts the same?" without a row store, stopping at the first
//! important difference (plan.md §5.4, backlog E4-S7). Used by folder content compare.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use core_base::{CancelToken, Result};
use core_text::{LoadOptions, TextDocument};

use super::compare::{Classifier, binary_equal};
use super::rows::RowKind;
use crate::align::{AlignOp, AlignRange, DocSource, align};
use crate::options::TextOptions;

/// The outcome of a verdict compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// The files are identical.
    Same,
    /// Only unimportant text differs: whitespace, comments, line endings, encoding or BOM.
    UnimportantOnly,
    /// An important difference exists.
    Different,
}

/// Compares two loaded documents under the options. Stops at the first important row.
///
/// If every row is `Same` but the files are not byte-identical (different line endings,
/// encoding or BOM) the verdict is `UnimportantOnly`, never a misleading `Same`. A binary
/// document cannot be compared as text and gives `Different`.
pub fn compare_verdict(
    left: &TextDocument,
    right: &TextDocument,
    opts: &TextOptions,
    cancel: &CancelToken,
) -> Result<Verdict> {
    if left.is_binary() || right.is_binary() {
        return Ok(Verdict::Different);
    }
    let rules = opts.line_rules()?;
    let source = DocSource::new(left, right, &rules, None);
    let classifier = Classifier {
        source: &source,
        orphans_sig: opts.orphans_always_sig,
    };
    // a child token: stopping early must not cancel the caller's token
    let stop = cancel.child();
    let found_important = AtomicBool::new(false);
    let mut unimportant = false;
    let mut ops_seen = 0u64;
    let result = align(
        &source,
        &opts.align_config(),
        AlignRange::full(),
        &stop,
        &mut |op: AlignOp| {
            ops_seen += 1;
            let mut rows = Vec::new();
            let mut c = Classifier {
                source: classifier.source,
                orphans_sig: classifier.orphans_sig,
            };
            c.rows(op, &mut rows);
            for r in rows {
                if r.kind.is_important() {
                    found_important.store(true, Ordering::Relaxed);
                    stop.cancel();
                    return;
                }
                unimportant |= r.kind != RowKind::Same;
            }
        },
    );
    let _ = ops_seen;
    if found_important.load(Ordering::Relaxed) {
        return Ok(Verdict::Different);
    }
    result?;
    if unimportant {
        return Ok(Verdict::UnimportantOnly);
    }
    // every row is Same: identical bytes?
    let identical = !left.is_lossy()
        && !right.is_lossy()
        && left.line_count() == right.line_count()
        && left.encoding() == right.encoding()
        && left.has_bom() == right.has_bom()
        && left.serialize().ok() == right.serialize().ok();
    Ok(if identical {
        Verdict::Same
    } else {
        Verdict::UnimportantOnly
    })
}

/// Verdict for two files. Binary files are compared byte for byte (`Same` or `Different`).
pub fn compare_verdict_files(
    left: &Path,
    right: &Path,
    opts: &TextOptions,
    load: &LoadOptions,
    cancel: &CancelToken,
) -> Result<Verdict> {
    let (l, r) = (
        TextDocument::load(left, load)?,
        TextDocument::load(right, load)?,
    );
    if l.is_binary() || r.is_binary() {
        return Ok(if binary_equal(left, right, cancel)? {
            Verdict::Same
        } else {
            Verdict::Different
        });
    }
    compare_verdict(&l, &r, opts, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_text::encoding::DecodeOptions;

    fn doc(bytes: &[u8]) -> TextDocument {
        TextDocument::from_bytes(bytes.to_vec(), &DecodeOptions::default()).unwrap()
    }

    fn verdict(a: &[u8], b: &[u8], opts: &TextOptions) -> Verdict {
        compare_verdict(&doc(a), &doc(b), opts, &CancelToken::new()).unwrap()
    }

    #[test]
    fn identical_bytes_are_same() {
        assert_eq!(
            verdict(b"a\nb\n", b"a\nb\n", &TextOptions::default()),
            Verdict::Same
        );
        assert_eq!(verdict(b"", b"", &TextOptions::default()), Verdict::Same);
    }

    /// Backlog E4-S7 golden: CRLF vs LF files give UnimportantOnly.
    #[test]
    fn crlf_versus_lf_is_unimportant_only() {
        assert_eq!(
            verdict(b"a\r\nb\r\n", b"a\nb\n", &TextOptions::default()),
            Verdict::UnimportantOnly
        );
    }

    #[test]
    fn bom_and_encoding_differences_are_unimportant_only() {
        assert_eq!(
            verdict(b"\xEF\xBB\xBFa\n", b"a\n", &TextOptions::default()),
            Verdict::UnimportantOnly
        );
        let mut u16 = vec![0xFF, 0xFE];
        u16.extend("a\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            verdict(&u16, b"a\n", &TextOptions::default()),
            Verdict::UnimportantOnly
        );
        // a missing final newline is an ending difference too
        assert_eq!(
            verdict(b"a\nb", b"a\nb\n", &TextOptions::default()),
            Verdict::UnimportantOnly
        );
    }

    #[test]
    fn whitespace_and_comment_only_changes_are_unimportant_only() {
        assert_eq!(
            verdict(b"  a\n", b"a\n", &TextOptions::default()),
            Verdict::UnimportantOnly
        );
        let o = TextOptions {
            unimportant: vec!["//.*$".into()],
            ..TextOptions::default()
        };
        assert_eq!(
            verdict(b"x // one\n", b"x // two\n", &o),
            Verdict::UnimportantOnly
        );
    }

    #[test]
    fn an_important_difference_is_different() {
        assert_eq!(
            verdict(b"a\nb\n", b"a\nc\n", &TextOptions::default()),
            Verdict::Different
        );
        assert_eq!(
            verdict(b"a\n", b"a\nextra\n", &TextOptions::default()),
            Verdict::Different
        );
        assert_eq!(
            verdict(b"a\n", b"", &TextOptions::default()),
            Verdict::Different
        );
    }

    /// Stops at the first important difference: with a difference at the very top of two
    /// huge texts the call returns long before an alignment of everything could.
    #[test]
    fn it_stops_early_and_leaves_the_callers_token_alone() {
        let big_a: String = (0..200_000)
            .map(|i| format!("a line number {i}\n"))
            .collect();
        let big_b: String = std::iter::once("DIFFERENT FIRST LINE\n".to_string())
            .chain((0..200_000).map(|i| format!("b other text {i}\n")))
            .collect();
        let cancel = CancelToken::new();
        let t = std::time::Instant::now();
        let v = compare_verdict(
            &doc(big_a.as_bytes()),
            &doc(big_b.as_bytes()),
            &TextOptions::default(),
            &cancel,
        )
        .unwrap();
        assert_eq!(v, Verdict::Different);
        assert!(
            !cancel.is_cancelled(),
            "early stop must not cancel the caller's token"
        );
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn cancelling_returns_cancelled() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let r = compare_verdict(&doc(b"a\n"), &doc(b"a\n"), &TextOptions::default(), &cancel);
        assert!(r.unwrap_err().is_cancelled());
    }

    #[test]
    fn binary_documents_are_different_and_files_compare_bytewise() {
        assert_eq!(
            verdict(b"\0\x01", b"\0\x01", &TextOptions::default()),
            Verdict::Different
        );
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a.bin"),
            dir.path().join("b.bin"),
            dir.path().join("c.bin"),
        );
        std::fs::write(&a, b"\0\x01\x02").unwrap();
        std::fs::write(&b, b"\0\x01\x02").unwrap();
        std::fs::write(&c, b"\0\x01\x03").unwrap();
        let (o, l, k) = (
            TextOptions::default(),
            LoadOptions::default(),
            CancelToken::new(),
        );
        assert_eq!(
            compare_verdict_files(&a, &b, &o, &l, &k).unwrap(),
            Verdict::Same
        );
        assert_eq!(
            compare_verdict_files(&a, &c, &o, &l, &k).unwrap(),
            Verdict::Different
        );
    }
}
