//! Display rows, sections, statistics and the progressive row store (plan.md §5.4,
//! backlog E4-S2, E4-S3, E4-S6).
//!
//! The aligner worker appends rows in batches; readers copy at most a viewport of rows under a
//! short read lock. Rows, sections and statistics live behind one lock, so a reader always sees
//! statistics that belong to exactly the rows it can read.

use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;

use crate::align::ORPHAN;

/// What a display row is. The numeric values are part of the C ABI and never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RowKind {
    Same = 0,
    /// Paired, only unimportant text differs.
    Unimportant = 1,
    /// Paired, an important difference.
    Different = 2,
    LeftOnly = 3,
    RightOnly = 4,
    /// An insignificant (blank or comment-only) left orphan.
    UnimportantLeftOnly = 5,
    UnimportantRightOnly = 6,
}

impl RowKind {
    pub fn is_difference(self) -> bool {
        self != RowKind::Same
    }

    pub fn is_important(self) -> bool {
        matches!(
            self,
            RowKind::Different | RowKind::LeftOnly | RowKind::RightOnly
        )
    }

    /// The name used by the fixture files (`expected.rows`).
    pub fn name(self) -> &'static str {
        match self {
            RowKind::Same => "Same",
            RowKind::Unimportant => "Unimportant",
            RowKind::Different => "Different",
            RowKind::LeftOnly => "LeftOnly",
            RowKind::RightOnly => "RightOnly",
            RowKind::UnimportantLeftOnly => "UnimportantLeftOnly",
            RowKind::UnimportantRightOnly => "UnimportantRightOnly",
        }
    }

    /// Display severity for overview minimaps: 0 same, 1 unimportant, 2 important.
    pub fn severity(self) -> u8 {
        match self {
            RowKind::Same => 0,
            RowKind::Different | RowKind::LeftOnly | RowKind::RightOnly => 2,
            _ => 1,
        }
    }
}

/// One display row: a line index on each side, `u32::MAX` when absent. 12 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DiffRow {
    pub left: u32,
    pub right: u32,
    pub kind: RowKind,
}

const _: () = assert!(size_of::<DiffRow>() == 12);

impl DiffRow {
    pub fn left_opt(&self) -> Option<u32> {
        (self.left != ORPHAN).then_some(self.left)
    }

    pub fn right_opt(&self) -> Option<u32> {
        (self.right != ORPHAN).then_some(self.right)
    }
}

/// A run of consecutive difference rows (between two `Same` rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Section {
    pub first_row: u32,
    pub row_count: u32,
    /// True when any row of the section is important.
    pub important: bool,
}

/// The nine counters of the original's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub same: u32,
    pub important_diff: u32,
    pub unimportant_diff: u32,
    pub left_orphan: u32,
    pub right_orphan: u32,
    pub unimportant_left_orphan: u32,
    pub unimportant_right_orphan: u32,
    pub sections: u32,
    pub important_sections: u32,
}

impl Stats {
    pub fn has_important_differences(&self) -> bool {
        self.important_diff + self.left_orphan + self.right_orphan > 0
    }

    pub fn has_any_differences(&self) -> bool {
        self.has_important_differences()
            || self.unimportant_diff + self.unimportant_left_orphan + self.unimportant_right_orphan
                > 0
    }
}

/// Direction of [`RowStore::find_diff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Next,
    Prev,
}

#[derive(Debug, Default)]
struct Inner {
    rows: Vec<DiffRow>,
    /// Closed sections; the last section stays open in `open` until a `Same` row or the end.
    closed: Vec<Section>,
    open: Option<Section>,
    closed_important: u32,
    stats: Stats,
}

impl Inner {
    fn push(&mut self, row: DiffRow) {
        let idx = self.rows.len() as u32;
        self.rows.push(row);
        match row.kind {
            RowKind::Same => self.stats.same += 1,
            RowKind::Unimportant => self.stats.unimportant_diff += 1,
            RowKind::Different => self.stats.important_diff += 1,
            RowKind::LeftOnly => self.stats.left_orphan += 1,
            RowKind::RightOnly => self.stats.right_orphan += 1,
            RowKind::UnimportantLeftOnly => self.stats.unimportant_left_orphan += 1,
            RowKind::UnimportantRightOnly => self.stats.unimportant_right_orphan += 1,
        }
        if row.kind.is_difference() {
            let sec = self.open.get_or_insert(Section {
                first_row: idx,
                row_count: 0,
                important: false,
            });
            sec.row_count += 1;
            sec.important |= row.kind.is_important();
        } else if let Some(sec) = self.open.take() {
            self.closed_important += u32::from(sec.important);
            self.closed.push(sec);
        }
        self.refresh_section_stats();
    }

    fn refresh_section_stats(&mut self) {
        let open = self.open.iter();
        self.stats.sections = self.closed.len() as u32 + open.len() as u32;
        self.stats.important_sections =
            self.closed_important + self.open.iter().filter(|s| s.important).count() as u32;
    }

    fn section_at(&self, i: usize) -> Option<Section> {
        self.closed
            .get(i)
            .copied()
            .or_else(|| (i == self.closed.len()).then_some(self.open).flatten())
    }

    fn section_count(&self) -> usize {
        self.closed.len() + usize::from(self.open.is_some())
    }
}

/// Append-only, progressively published rows with their sections and statistics.
#[derive(Debug, Default)]
pub struct RowStore {
    inner: RwLock<Inner>,
    published: AtomicU32,
}

impl RowStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a batch under one write lock and publishes the new count.
    pub fn append(&self, batch: &[DiffRow]) {
        if batch.is_empty() {
            return;
        }
        let mut g = self.inner.write();
        g.rows.reserve(batch.len());
        for &r in batch {
            g.push(r);
        }
        self.published.store(g.rows.len() as u32, Ordering::Release);
    }

    pub fn clear(&self) {
        let mut g = self.inner.write();
        *g = Inner::default();
        self.published.store(0, Ordering::Release);
    }

    /// Number of rows readers can see.
    pub fn published(&self) -> u32 {
        self.published.load(Ordering::Acquire)
    }

    /// Copies rows `first..` into `out`; returns how many were copied.
    pub fn rows(&self, first: u32, out: &mut [DiffRow]) -> usize {
        let g = self.inner.read();
        let first = first as usize;
        if first >= g.rows.len() {
            return 0;
        }
        let n = out.len().min(g.rows.len() - first);
        out[..n].copy_from_slice(&g.rows[first..first + n]);
        n
    }

    /// Copies sections `first..` (the open one last) into `out`.
    pub fn sections(&self, first: u32, out: &mut [Section]) -> usize {
        let g = self.inner.read();
        let total = g.section_count();
        let first = first as usize;
        if first >= total {
            return 0;
        }
        let n = out.len().min(total - first);
        for (k, slot) in out[..n].iter_mut().enumerate() {
            *slot = g.section_at(first + k).unwrap_or(Section {
                first_row: 0,
                row_count: 0,
                important: false,
            });
        }
        n
    }

    /// Statistics consistent with the rows published at the same instant.
    pub fn stats(&self) -> Stats {
        self.inner.read().stats
    }

    /// Row count and statistics from one snapshot.
    pub fn snapshot(&self) -> (u32, Stats) {
        let g = self.inner.read();
        (g.rows.len() as u32, g.stats)
    }

    /// The start row of the next (or previous) section after (or before) `from_row`.
    pub fn find_diff(&self, from_row: u32, dir: Dir, important_only: bool) -> Option<u32> {
        let g = self.inner.read();
        let n = g.section_count();
        let ok = |s: &Section| !important_only || s.important;
        match dir {
            Dir::Next => {
                // first section starting after from_row (binary search on the start rows)
                let (mut lo, mut hi) = (0, n);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if g.section_at(mid).is_some_and(|s| s.first_row <= from_row) {
                        lo = mid + 1
                    } else {
                        hi = mid
                    }
                }
                (lo..n)
                    .filter_map(|i| g.section_at(i))
                    .find(ok)
                    .map(|s| s.first_row)
            }
            Dir::Prev => {
                let (mut lo, mut hi) = (0, n);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if g.section_at(mid).is_some_and(|s| s.first_row < from_row) {
                        lo = mid + 1
                    } else {
                        hi = mid
                    }
                }
                (0..lo)
                    .rev()
                    .filter_map(|i| g.section_at(i))
                    .find(ok)
                    .map(|s| s.first_row)
            }
        }
    }

    /// Minimap: for every bucket the worst row kind in its slice of the rows (important beats
    /// unimportant beats same; the first row of the worst severity decides). Buckets of an
    /// empty store are `Same`.
    pub fn overview(&self, buckets: &mut [u8]) {
        let g = self.inner.read();
        let n = g.rows.len();
        let b = buckets.len();
        for (k, slot) in buckets.iter_mut().enumerate() {
            let (lo, hi) = (k * n / b.max(1), (k + 1) * n / b.max(1));
            let mut worst = RowKind::Same;
            for r in &g.rows[lo..hi.max(lo)] {
                if r.kind.severity() > worst.severity() {
                    worst = r.kind;
                    if worst.severity() == 2 {
                        break;
                    }
                }
            }
            *slot = worst as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(i: u32, kind: RowKind) -> DiffRow {
        DiffRow {
            left: i,
            right: i,
            kind,
        }
    }

    fn kinds(spec: &str) -> Vec<DiffRow> {
        spec.chars()
            .enumerate()
            .map(|(i, c)| {
                row(
                    i as u32,
                    match c {
                        '.' => RowKind::Same,
                        'u' => RowKind::Unimportant,
                        'd' => RowKind::Different,
                        'L' => RowKind::LeftOnly,
                        'R' => RowKind::RightOnly,
                        _ => panic!(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn row_layout_and_abi_values_are_fixed() {
        assert_eq!(size_of::<DiffRow>(), 12);
        assert_eq!(
            [
                RowKind::Same,
                RowKind::Unimportant,
                RowKind::Different,
                RowKind::LeftOnly,
                RowKind::RightOnly,
                RowKind::UnimportantLeftOnly,
                RowKind::UnimportantRightOnly
            ]
            .map(|k| k as u8),
            [0, 1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn sections_are_built_incrementally_and_the_last_stays_open() {
        let store = RowStore::new();
        store.append(&kinds("..dd"));
        let mut s = [Section {
            first_row: 0,
            row_count: 0,
            important: false,
        }; 4];
        assert_eq!(store.sections(0, &mut s), 1);
        assert_eq!(
            s[0],
            Section {
                first_row: 2,
                row_count: 2,
                important: true
            }
        );
        assert_eq!(store.stats().sections, 1, "the open section counts");
        store.append(&kinds("."));
        assert_eq!(store.stats().sections, 1);
        store.append(&kinds("u"));
        assert_eq!(store.sections(0, &mut s), 2);
        assert_eq!(
            s[1],
            Section {
                first_row: 5,
                row_count: 1,
                important: false
            }
        );
        let st = store.stats();
        assert_eq!((st.sections, st.important_sections), (2, 1));
    }

    /// Incremental sections equal a batch recompute (backlog E4-S3.1).
    #[test]
    fn incremental_sections_equal_a_batch_recompute() {
        let spec = "..dd.u.RL..uuu.d.d.R";
        let rows = kinds(spec);
        for chunk in [1usize, 2, 3, 7, 100] {
            let store = RowStore::new();
            for c in rows.chunks(chunk) {
                store.append(c);
            }
            let mut got = [Section {
                first_row: 0,
                row_count: 0,
                important: false,
            }; 16];
            let n = store.sections(0, &mut got);
            // batch recompute
            let mut want: Vec<Section> = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                if r.kind.is_difference() {
                    match want.last_mut() {
                        Some(s) if s.first_row + s.row_count == i as u32 => {
                            s.row_count += 1;
                            s.important |= r.kind.is_important();
                        }
                        _ => want.push(Section {
                            first_row: i as u32,
                            row_count: 1,
                            important: r.kind.is_important(),
                        }),
                    }
                }
            }
            assert_eq!(&got[..n], want.as_slice(), "chunk {chunk}");
            assert_eq!(store.stats().sections as usize, want.len());
            assert_eq!(
                store.stats().important_sections as usize,
                want.iter().filter(|s| s.important).count()
            );
        }
    }

    #[test]
    fn stats_count_every_kind() {
        let store = RowStore::new();
        store.append(&kinds("..udLR"));
        store.append(&[
            row(6, RowKind::UnimportantLeftOnly),
            row(7, RowKind::UnimportantRightOnly),
        ]);
        let s = store.stats();
        assert_eq!(
            (
                s.same,
                s.important_diff,
                s.unimportant_diff,
                s.left_orphan,
                s.right_orphan,
                s.unimportant_left_orphan,
                s.unimportant_right_orphan
            ),
            (2, 1, 1, 1, 1, 1, 1)
        );
        assert!(s.has_important_differences() && s.has_any_differences());
        assert!(!Stats::default().has_any_differences());
    }

    #[test]
    fn rows_copy_a_viewport_and_clamp() {
        let store = RowStore::new();
        store.append(&kinds("..dd.."));
        let mut out = [row(0, RowKind::Same); 4];
        assert_eq!(store.rows(1, &mut out), 4);
        assert_eq!(out[1].kind, RowKind::Different);
        assert_eq!(store.rows(4, &mut out), 2);
        assert_eq!(store.rows(6, &mut out), 0);
        assert_eq!(store.rows(100, &mut out), 0);
        assert_eq!(store.published(), 6);
    }

    #[test]
    fn find_diff_edges() {
        let store = RowStore::new();
        assert_eq!(store.find_diff(0, Dir::Next, false), None, "no rows");
        store.append(&kinds("..d..u..d."));
        // sections start at rows 2, 5, 8; the one at 5 is unimportant
        assert_eq!(store.find_diff(0, Dir::Next, false), Some(2));
        assert_eq!(
            store.find_diff(2, Dir::Next, false),
            Some(5),
            "strictly after the current row"
        );
        assert_eq!(
            store.find_diff(2, Dir::Next, true),
            Some(8),
            "unimportant sections are skipped"
        );
        assert_eq!(
            store.find_diff(8, Dir::Next, false),
            None,
            "last section: nothing after"
        );
        assert_eq!(store.find_diff(9, Dir::Prev, false), Some(8));
        assert_eq!(store.find_diff(8, Dir::Prev, true), Some(2));
        assert_eq!(
            store.find_diff(2, Dir::Prev, false),
            None,
            "first section: nothing before"
        );
        assert_eq!(store.find_diff(0, Dir::Prev, false), None);
        let none = RowStore::new();
        none.append(&kinds("...."));
        assert_eq!(none.find_diff(0, Dir::Next, false), None, "no diffs");
    }

    #[test]
    fn overview_reports_the_worst_kind_per_bucket() {
        let store = RowStore::new();
        store.append(&kinds("....u...d..."));
        let mut b = [9u8; 4];
        store.overview(&mut b);
        assert_eq!(b, [0, 1, 2, 0]);
        // one bucket sees everything
        let mut one = [9u8; 1];
        store.overview(&mut one);
        assert_eq!(one[0], RowKind::Different as u8);
        // more buckets than rows: still defined
        let mut many = [9u8; 30];
        store.overview(&mut many);
        assert!(many.iter().all(|&k| k <= 6));
        let mut empty = [9u8; 3];
        RowStore::new().overview(&mut empty);
        assert_eq!(empty, [0, 0, 0]);
    }

    #[test]
    fn overview_with_10k_buckets_over_a_big_store() {
        let store = RowStore::new();
        let rows: Vec<DiffRow> = (0..100_000)
            .map(|i| {
                row(
                    i,
                    if i % 5000 == 7 {
                        RowKind::Different
                    } else {
                        RowKind::Same
                    },
                )
            })
            .collect();
        store.append(&rows);
        let mut b = vec![0u8; 10_000];
        store.overview(&mut b);
        assert_eq!(
            b.iter().filter(|&&k| k == RowKind::Different as u8).count(),
            20
        );
    }

    /// Readers see statistics that belong to the rows they read, under concurrent appends
    /// (backlog E4-S3.2).
    #[test]
    fn a_reader_never_sees_stats_from_another_snapshot() {
        use std::sync::Arc;
        let store = Arc::new(RowStore::new());
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..4000u32 {
                    let kind = if i % 3 == 0 {
                        RowKind::Different
                    } else {
                        RowKind::Same
                    };
                    store.append(&[row(i, kind)]);
                }
            })
        };
        while !writer.is_finished() {
            let (n, s) = store.snapshot();
            let total = s.same + s.important_diff;
            assert_eq!(total, n, "stats and row count come from one snapshot");
        }
        writer.join().unwrap();
        assert_eq!(store.published(), 4000);
    }

    #[test]
    fn clear_resets_everything() {
        let store = RowStore::new();
        store.append(&kinds("dd."));
        store.clear();
        assert_eq!(store.published(), 0);
        assert_eq!(store.stats(), Stats::default());
        assert_eq!(store.find_diff(0, Dir::Next, false), None);
    }
}
