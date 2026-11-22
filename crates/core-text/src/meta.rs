//! Per-line alignment metadata (plan.md §4.3, spec ALIGN-1.2, backlog E1-S6).
//!
//! Each line's metadata is a pure function of the line, the rules and the weight hook, so it
//! can be computed lazily in chunks (`MetaCursor`) or ahead of time in parallel
//! ([`compute_all_parallel`]) with identical results.

use core_base::crc32::line_hash_str;
use rayon::prelude::*;

use crate::document::TextDocument;
use crate::rules::{LineRules, Scratch};

/// Lines are loaded in chunks of at least this many (spec ALIGN-1.2: "arrays grow in steps
/// of 128").
pub const META_CHUNK: usize = 128;

/// Weight of an empty line, spec ALIGN-1.2.
pub const WEIGHT_EMPTY: i32 = 2;
/// Default weight base `w` of a significant line (`2w + 50` = 70).
pub const DEFAULT_WEIGHT_SIG: i32 = 10;
/// Default weight base `w` of an insignificant line (`w + 30` = 31).
pub const DEFAULT_WEIGHT_INSIG: i32 = 1;

const SIG_BIT: u32 = 1 << 31;

/// A custom weight hook: extra weight for a *full* line, `0` = use the default. Must be pure.
pub type WeightFn<'a> = &'a (dyn Fn(&str) -> i32 + Sync);

/// 12 bytes of metadata per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct LineMeta {
    /// CRC line hash of the comparable text, or of the full line when that is empty.
    pub hash: u32,
    /// 2 for an empty line, 31 insignificant, 70 significant (custom weights aware).
    pub weight: i32,
    /// UTF-8 length of the comparable text; bit 31 is the *significant* flag.
    cmp_len: u32,
}

const _: () = assert!(size_of::<LineMeta>() == 12);

impl LineMeta {
    pub fn new(hash: u32, weight: i32, cmp_len: u32, is_sig: bool) -> Self {
        let len = cmp_len.min(!SIG_BIT);
        LineMeta {
            hash,
            weight,
            cmp_len: len | if is_sig { SIG_BIT } else { 0 },
        }
    }

    /// A significant line has a non-empty comparable text.
    pub fn is_sig(&self) -> bool {
        self.cmp_len & SIG_BIT != 0
    }

    /// UTF-8 length of the comparable text (0 for insignificant lines).
    pub fn cmp_len(&self) -> u32 {
        self.cmp_len & !SIG_BIT
    }

    /// Metadata of one line under `rules` (spec ALIGN-1.2).
    ///
    /// * comparable text = the line after the rules; empty → insignificant;
    /// * hash = line hash of the comparable text, or of the full line when it is empty;
    /// * weight: empty full line → 2; else `w` = hook result (0 = default 10 / 1), then
    ///   significant → `2w + 50`, insignificant → `w + 30`.
    pub fn compute(
        line: &str,
        rules: &LineRules,
        scratch: &mut Scratch,
        weight_hook: Option<WeightFn<'_>>,
    ) -> Self {
        let cmp = rules.normalize(line, scratch);
        let sig = !cmp.is_empty();
        let hash = line_hash_str(if sig { cmp } else { line });
        let cmp_len = cmp.len();
        let weight = if line.is_empty() {
            WEIGHT_EMPTY
        } else {
            // A negative hook result is treated as "no opinion", like 0.
            let mut w = weight_hook.map_or(0, |f| f(line)).max(0);
            if w == 0 {
                w = if sig {
                    DEFAULT_WEIGHT_SIG
                } else {
                    DEFAULT_WEIGHT_INSIG
                };
            }
            if sig {
                w.saturating_mul(2).saturating_add(50)
            } else {
                w.saturating_add(30)
            }
        };
        LineMeta::new(
            hash,
            weight,
            u32::try_from(cmp_len).unwrap_or(u32::MAX),
            sig,
        )
    }
}

/// Lazily computes the metadata of one document's lines, in chunks of at least
/// [`META_CHUNK`], and keeps the results.
pub struct MetaCursor<'a> {
    doc: &'a TextDocument,
    rules: &'a LineRules,
    hook: Option<WeightFn<'a>>,
    metas: Vec<LineMeta>,
    scratch: Scratch,
}

impl std::fmt::Debug for MetaCursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaCursor")
            .field("lines", &self.len())
            .field("computed", &self.metas.len())
            .field("has_hook", &self.hook.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> MetaCursor<'a> {
    pub fn new(doc: &'a TextDocument, rules: &'a LineRules, hook: Option<WeightFn<'a>>) -> Self {
        MetaCursor {
            doc,
            rules,
            hook,
            metas: Vec::new(),
            scratch: Scratch::new(),
        }
    }

    /// Number of lines of the document.
    pub fn len(&self) -> usize {
        self.doc.line_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many lines have been computed so far.
    pub fn computed(&self) -> usize {
        self.metas.len()
    }

    /// Makes sure lines `0..=idx` are computed (in chunks); out-of-range indexes are clamped.
    pub fn ensure(&mut self, idx: usize) {
        let total = self.len();
        if idx >= total || idx < self.metas.len() {
            return;
        }
        let target = (idx + 1).max(self.metas.len() + META_CHUNK).min(total);
        self.metas.reserve(target - self.metas.len());
        for i in self.metas.len()..target {
            let mut m =
                LineMeta::compute(self.doc.line(i), self.rules, &mut self.scratch, self.hook);
            m.hash ^= self.rules.eol_salt(self.doc.line_ending(i));
            self.metas.push(m);
        }
    }

    /// Metadata of line `idx`. Panics when `idx` is out of range.
    pub fn get(&mut self, idx: usize) -> LineMeta {
        assert!(
            idx < self.len(),
            "line {idx} out of range ({} lines)",
            self.len()
        );
        self.ensure(idx);
        self.metas[idx]
    }
}

/// Computes every line's metadata on the rayon pool. The result equals what a
/// [`MetaCursor`] produces line by line.
pub fn compute_all_parallel(
    doc: &TextDocument,
    rules: &LineRules,
    hook: Option<WeightFn<'_>>,
) -> Vec<LineMeta> {
    (0..doc.line_count())
        .into_par_iter()
        .with_min_len(1024)
        .map_init(Scratch::new, |scratch, i| {
            let mut m = LineMeta::compute(doc.line(i), rules, scratch, hook);
            m.hash ^= rules.eol_salt(doc.line_ending(i));
            m
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::WhitespaceMode;

    fn meta(line: &str) -> LineMeta {
        LineMeta::compute(line, &LineRules::new(), &mut Scratch::new(), None)
    }

    /// Hash values from `refmodel.py hash`; weights from ALIGN-1.2.
    #[test]
    fn matches_the_reference_model_on_golden_inputs() {
        // significant
        let m = meta("a");
        assert_eq!(
            (m.hash, m.weight, m.is_sig(), m.cmp_len()),
            (0xC2C0_B7E6, 70, true, 1)
        );
        let m = meta("abc");
        assert_eq!((m.hash, m.weight, m.cmp_len()), (0x526A_854F, 70, 3));
        let m = meta("שלום");
        assert_eq!((m.hash, m.weight, m.cmp_len()), (0xAADB_A6C2, 70, 8)); // 4 chars × 2 bytes
        let m = meta("\u{1F600}");
        assert_eq!((m.hash, m.weight, m.cmp_len()), (0x3E0B_9BC4, 70, 4));
        // The hash is of the *comparable* text: indentation does not change it (G9).
        assert_eq!(meta("  x  ").hash, meta("x").hash);
        assert_eq!(meta("  x  ").cmp_len(), 1);
    }

    #[test]
    fn empty_line_weighs_two_and_hashes_to_all_ones() {
        let m = meta("");
        assert_eq!(
            (m.hash, m.weight, m.is_sig(), m.cmp_len()),
            (0xFFFF_FFFF, 2, false, 0)
        );
    }

    #[test]
    fn blank_line_is_insignificant_and_hashes_its_full_text() {
        let m = meta("  x  ".trim()); // sanity: significant
        assert!(m.is_sig());
        let m = meta("   ");
        assert!(!m.is_sig());
        assert_eq!(m.weight, 31); // insignificant: w + 30 with w = 1
        assert_eq!(m.hash, core_base::crc32::line_hash_str("   "));
        assert_eq!(m.cmp_len(), 0);
    }

    #[test]
    fn comment_only_line_is_insignificant() {
        let rules = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let m = LineMeta::compute("  // note", &rules, &mut Scratch::new(), None);
        assert!(!m.is_sig());
        assert_eq!(m.weight, 31);
        assert_eq!(m.hash, core_base::crc32::line_hash_str("  // note"));
    }

    #[test]
    fn custom_weight_hook_replaces_the_default_base() {
        let hook = |s: &str| if s.starts_with("fn ") { 70 } else { 0 };
        let mut sc = Scratch::new();
        let rules = LineRules::new();
        let m = LineMeta::compute("fn main()", &rules, &mut sc, Some(&hook));
        assert_eq!(m.weight, 2 * 70 + 50);
        // 0 = default
        let m = LineMeta::compute("let x", &rules, &mut sc, Some(&hook));
        assert_eq!(m.weight, 70);
        // insignificant line with a custom weight: w + 30
        let hook5 = |_: &str| 5;
        let m = LineMeta::compute("   ", &rules, &mut sc, Some(&hook5));
        assert_eq!(m.weight, 35);
        // the empty line ignores the hook
        let m = LineMeta::compute("", &rules, &mut sc, Some(&hook5));
        assert_eq!(m.weight, 2);
        // negative = no opinion
        let neg = |_: &str| -9;
        assert_eq!(
            LineMeta::compute("x", &rules, &mut sc, Some(&neg)).weight,
            70
        );
    }

    #[test]
    fn is_sig_bit_does_not_disturb_the_length() {
        let m = LineMeta::new(1, 2, 12345, true);
        assert!(m.is_sig());
        assert_eq!(m.cmp_len(), 12345);
        let m = LineMeta::new(1, 2, u32::MAX, false);
        assert!(!m.is_sig());
        assert_eq!(m.cmp_len(), 0x7FFF_FFFF);
        assert_eq!(size_of::<LineMeta>(), 12);
    }

    #[test]
    fn ignore_case_and_whitespace_modes_change_the_hash() {
        let r = LineRules::new()
            .with_ignore_case(true)
            .with_whitespace(WhitespaceMode::IgnoreAll);
        let mut sc = Scratch::new();
        let a = LineMeta::compute("Foo Bar", &r, &mut sc, None);
        let b = LineMeta::compute("fooBAR", &r, &mut sc, None);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.cmp_len(), 6);
    }

    #[test]
    fn cursor_computes_lazily_in_chunks_of_at_least_128() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let doc = TextDocument::from_text(&text).unwrap();
        let rules = LineRules::new();
        let mut cur = MetaCursor::new(&doc, &rules, None);
        assert_eq!((cur.len(), cur.computed()), (1000, 0));
        cur.get(0);
        assert_eq!(cur.computed(), 128);
        cur.get(127);
        assert_eq!(cur.computed(), 128);
        cur.get(128);
        assert_eq!(cur.computed(), 256);
        cur.get(999);
        assert_eq!(cur.computed(), 1000);
    }

    #[test]
    fn cursor_matches_direct_computation() {
        let doc = TextDocument::from_text("a\n\n  b  \n// c\nא\n").unwrap();
        let rules = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let mut cur = MetaCursor::new(&doc, &rules, None);
        let mut sc = Scratch::new();
        for i in (0..doc.line_count()).rev() {
            assert_eq!(
                cur.get(i),
                LineMeta::compute(doc.line(i), &rules, &mut sc, None)
            );
        }
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn cursor_panics_past_the_end() {
        let doc = TextDocument::from_text("a\n").unwrap();
        let rules = LineRules::new();
        MetaCursor::new(&doc, &rules, None).get(1);
    }

    #[test]
    fn parallel_precompute_equals_lazy_cursor() {
        let text: String = (0..5000)
            .map(|i| match i % 5 {
                0 => format!("  indented {i}\n"),
                1 => "\n".to_string(),
                2 => format!("// comment {i}\n"),
                3 => format!("שלום {i}\n"),
                _ => format!("code {} /* c */\n", i % 17),
            })
            .collect();
        let doc = TextDocument::from_text(&text).unwrap();
        let rules = LineRules::new()
            .with_ignore_case(true)
            .with_unimportant(["//.*$", "/\\*.*?\\*/"])
            .unwrap();
        let par = compute_all_parallel(&doc, &rules, None);
        let mut cur = MetaCursor::new(&doc, &rules, None);
        assert_eq!(par.len(), doc.line_count());
        for (i, m) in par.iter().enumerate() {
            assert_eq!(*m, cur.get(i), "line {i}");
        }
    }
}
