//! Where the aligner reads its lines from (plan.md §5.1), and the ops it writes.

use core_text::TextDocument;
use core_text::meta::{LineMeta, MetaCursor, WeightFn};
use core_text::rules::{LineRules, Scratch};
use parking_lot::Mutex;

/// The two sides of a compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    /// The source side (`src`, the original's `asSrc`).
    Left = 0,
    /// The target side (`tgt`).
    Right = 1,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Left, Side::Right];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// One output step of the aligner: `amount` lines starting at `src` and `tgt` belong
/// together. [`ORPHAN`] on a side means the lines exist only on the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AlignOp {
    pub src: u32,
    pub tgt: u32,
    pub amount: u32,
}

/// Marks the missing side of an orphan [`AlignOp`] (and an absent line in a row).
pub const ORPHAN: u32 = u32::MAX;

impl AlignOp {
    pub fn paired(src: u32, tgt: u32, amount: u32) -> Self {
        AlignOp { src, tgt, amount }
    }

    pub fn src_orphans(src: u32, amount: u32) -> Self {
        AlignOp {
            src,
            tgt: ORPHAN,
            amount,
        }
    }

    pub fn tgt_orphans(tgt: u32, amount: u32) -> Self {
        AlignOp {
            src: ORPHAN,
            tgt,
            amount,
        }
    }

    pub fn src_opt(&self) -> Option<u32> {
        (self.src != ORPHAN).then_some(self.src)
    }

    pub fn tgt_opt(&self) -> Option<u32> {
        (self.tgt != ORPHAN).then_some(self.tgt)
    }

    pub fn is_paired(&self) -> bool {
        self.src != ORPHAN && self.tgt != ORPHAN
    }
}

impl std::fmt::Display for AlignOp {
    /// The `expected.ops` fixture format: `src tgt amount`, `-` for the orphan side.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let side = |v: Option<u32>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
        write!(
            f,
            "{} {} {}",
            side(self.src_opt()),
            side(self.tgt_opt()),
            self.amount
        )
    }
}

/// The original's `TBcAlignLines` provider. The aligner reads lines lazily and in order.
pub trait LineSource {
    /// True when line `idx` exists on `side` (0-based). May load more lines lazily.
    fn exists(&self, side: Side, idx: u32) -> bool;

    /// Alignment metadata of a line: hash, weight, significance (spec ALIGN-1.2). Called at
    /// most once per line by one aligner run.
    fn meta(&self, side: Side, idx: u32) -> LineMeta;

    /// The text closeness works on, as UTF-8: the comparable text, or the full line when the
    /// comparable text is empty. **Replaces** the content of `out`.
    fn closeness_text(&self, side: Side, idx: u32, out: &mut Vec<u8>);

    /// True when the *full* lines (not the comparable texts) are byte-identical (decision D2).
    fn full_text_eq(&self, src_idx: u32, tgt_idx: u32) -> bool;
}

/// A [`LineSource`] over two lists of strings, with metadata computed up front. Meant for
/// tests, fuzzing, `dsdev` and small inputs.
#[derive(Debug, Clone)]
pub struct VecSource {
    full: [Vec<String>; 2],
    comparable: [Vec<String>; 2],
    metas: [Vec<LineMeta>; 2],
}

impl VecSource {
    pub fn new<S: AsRef<str>>(left: &[S], right: &[S], rules: &LineRules) -> Self {
        Self::with_weights(left, right, rules, None)
    }

    pub fn with_weights<S: AsRef<str>>(
        left: &[S],
        right: &[S],
        rules: &LineRules,
        hook: Option<WeightFn<'_>>,
    ) -> Self {
        let mut scratch = Scratch::new();
        let mut build = |lines: &[S]| {
            let full: Vec<String> = lines.iter().map(|l| l.as_ref().to_owned()).collect();
            let mut comparable = Vec::with_capacity(full.len());
            let mut metas = Vec::with_capacity(full.len());
            for l in &full {
                comparable.push(rules.normalize(l, &mut scratch).to_owned());
                metas.push(LineMeta::compute(l, rules, &mut scratch, hook));
            }
            (full, comparable, metas)
        };
        let (lf, lc, lm) = build(left);
        let (rf, rc, rm) = build(right);
        VecSource {
            full: [lf, rf],
            comparable: [lc, rc],
            metas: [lm, rm],
        }
    }

    pub fn len(&self, side: Side) -> usize {
        self.full[side.index()].len()
    }

    pub fn is_empty(&self, side: Side) -> bool {
        self.full[side.index()].is_empty()
    }

    pub fn full(&self, side: Side, idx: u32) -> &str {
        &self.full[side.index()][idx as usize]
    }

    pub fn comparable(&self, side: Side, idx: u32) -> &str {
        &self.comparable[side.index()][idx as usize]
    }
}

impl LineSource for VecSource {
    fn exists(&self, side: Side, idx: u32) -> bool {
        (idx as usize) < self.full[side.index()].len()
    }

    fn meta(&self, side: Side, idx: u32) -> LineMeta {
        self.metas[side.index()][idx as usize]
    }

    fn closeness_text(&self, side: Side, idx: u32, out: &mut Vec<u8>) {
        out.clear();
        let i = side.index();
        let cmp = &self.comparable[i][idx as usize];
        let text = if cmp.is_empty() {
            &self.full[i][idx as usize]
        } else {
            cmp
        };
        out.extend_from_slice(text.as_bytes());
    }

    fn full_text_eq(&self, src_idx: u32, tgt_idx: u32) -> bool {
        self.full[0][src_idx as usize] == self.full[1][tgt_idx as usize]
    }
}

/// A [`LineSource`] over two loaded documents. Metadata is computed lazily in chunks (and
/// can be shared with row classification through [`DocSource::comparable_eq`]).
pub struct DocSource<'a> {
    docs: [&'a TextDocument; 2],
    rules: &'a LineRules,
    cursors: [Mutex<MetaCursor<'a>>; 2],
    scratch: Mutex<Scratch>,
}

impl std::fmt::Debug for DocSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocSource")
            .field("left_lines", &self.docs[0].line_count())
            .field("right_lines", &self.docs[1].line_count())
            .finish_non_exhaustive()
    }
}

impl<'a> DocSource<'a> {
    pub fn new(
        left: &'a TextDocument,
        right: &'a TextDocument,
        rules: &'a LineRules,
        hook: Option<WeightFn<'a>>,
    ) -> Self {
        DocSource {
            docs: [left, right],
            rules,
            cursors: [
                Mutex::new(MetaCursor::new(left, rules, hook)),
                Mutex::new(MetaCursor::new(right, rules, hook)),
            ],
            scratch: Mutex::new(Scratch::new()),
        }
    }

    pub fn doc(&self, side: Side) -> &'a TextDocument {
        self.docs[side.index()]
    }

    pub fn rules(&self) -> &'a LineRules {
        self.rules
    }

    /// True when the comparable texts of the two lines are equal (not just their hashes).
    pub fn comparable_eq(&self, left: u32, right: u32) -> bool {
        let mut scratch = self.scratch.lock();
        let a = self
            .rules
            .normalize(self.docs[0].line(left as usize), &mut scratch)
            .to_owned();
        let b = self
            .rules
            .normalize(self.docs[1].line(right as usize), &mut scratch);
        a == b
    }

    /// True when the comparable text of the line is empty (the line is insignificant).
    pub fn is_insignificant(&self, side: Side, idx: u32) -> bool {
        !self.cursors[side.index()].lock().get(idx as usize).is_sig()
    }
}

impl LineSource for DocSource<'_> {
    fn exists(&self, side: Side, idx: u32) -> bool {
        (idx as usize) < self.docs[side.index()].line_count()
    }

    fn meta(&self, side: Side, idx: u32) -> LineMeta {
        self.cursors[side.index()].lock().get(idx as usize)
    }

    fn closeness_text(&self, side: Side, idx: u32, out: &mut Vec<u8>) {
        out.clear();
        let line = self.docs[side.index()].line(idx as usize);
        let mut scratch = self.scratch.lock();
        let cmp = self.rules.normalize(line, &mut scratch);
        out.extend_from_slice(if cmp.is_empty() { line } else { cmp }.as_bytes());
    }

    fn full_text_eq(&self, src_idx: u32, tgt_idx: u32) -> bool {
        self.docs[0].line_bytes(src_idx as usize) == self.docs[1].line_bytes(tgt_idx as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_display_is_the_fixture_format() {
        assert_eq!(AlignOp::paired(0, 0, 1).to_string(), "0 0 1");
        assert_eq!(AlignOp::tgt_orphans(2, 1).to_string(), "- 2 1");
        assert_eq!(AlignOp::src_orphans(7, 200).to_string(), "7 - 200");
        assert!(AlignOp::paired(1, 2, 3).is_paired());
        assert!(!AlignOp::src_orphans(1, 1).is_paired());
        assert_eq!(AlignOp::tgt_orphans(2, 1).src_opt(), None);
    }

    #[test]
    fn vec_source_serves_metadata_and_texts() {
        let rules = LineRules::new();
        let s = VecSource::new(&["  a  ", "", "x y"], &["a"], &rules);
        assert!(s.exists(Side::Left, 2) && !s.exists(Side::Left, 3));
        assert!(s.exists(Side::Right, 0) && !s.exists(Side::Right, 1));
        let m = s.meta(Side::Left, 0);
        assert_eq!(
            m,
            s.meta(Side::Right, 0),
            "indentation does not change the metadata"
        );
        assert!(m.is_sig());
        let empty = s.meta(Side::Left, 1);
        assert!(!empty.is_sig());
        assert_eq!(empty.weight, 2);
        let mut buf = vec![9u8; 4];
        s.closeness_text(Side::Left, 0, &mut buf);
        assert_eq!(buf, b"a", "comparable text, previous content replaced");
        s.closeness_text(Side::Left, 1, &mut buf);
        assert_eq!(buf, b"", "an empty line has an empty closeness text");
        assert!(
            !s.full_text_eq(0, 0),
            "'  a  ' and 'a' differ as full lines"
        );
        assert_eq!(s.len(Side::Left), 3);
    }

    #[test]
    fn closeness_text_of_an_insignificant_line_is_the_full_line() {
        let rules = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let s = VecSource::new(&["  // note"], &["x"], &rules);
        let mut buf = Vec::new();
        s.closeness_text(Side::Left, 0, &mut buf);
        assert_eq!(buf, b"  // note");
        assert!(!s.meta(Side::Left, 0).is_sig());
    }

    #[test]
    fn doc_source_agrees_with_vec_source() {
        let l = ["  one", "two", "", "// c", "three"];
        let r = ["one", "two  ", "three"];
        let rules = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let vs = VecSource::new(&l, &r, &rules);
        let dl = TextDocument::from_text(&l.join("\n")).unwrap();
        let dr = TextDocument::from_text(&r.join("\n")).unwrap();
        let ds = DocSource::new(&dl, &dr, &rules, None);
        for (side, n) in [(Side::Left, l.len()), (Side::Right, r.len())] {
            assert!(!ds.exists(side, n as u32));
            for i in 0..n as u32 {
                assert_eq!(ds.meta(side, i), vs.meta(side, i), "{side:?} {i}");
                let (mut a, mut b) = (Vec::new(), Vec::new());
                ds.closeness_text(side, i, &mut a);
                vs.closeness_text(side, i, &mut b);
                assert_eq!(a, b);
            }
        }
        assert_eq!(ds.full_text_eq(1, 1), vs.full_text_eq(1, 1));
        assert!(
            ds.comparable_eq(1, 1),
            "'two' and 'two  ' have the same comparable text"
        );
        assert!(!ds.full_text_eq(1, 1));
        assert!(ds.is_insignificant(Side::Left, 3));
        assert!(!ds.is_insignificant(Side::Left, 4));
    }
}
