//! Constants of the alignment engine, each with its spec reference (plan.md Appendix A).
//! All arithmetic in the aligner is integer; scores are `i32`, run weights `i64`.

pub use core_text::meta::{
    DEFAULT_WEIGHT_INSIG, DEFAULT_WEIGHT_SIG, META_CHUNK as LINE_GROWTH_STEP, WEIGHT_EMPTY,
};

/// Default skew tolerance: the window is this many lines per side. spec: ALIGN-1.1.
pub const DEFAULT_SKEW: u32 = 2000;

/// Largest accepted skew tolerance (decision D41): the visited bitset of a window is
/// `skew^2 / 8` bytes, so 20 000 is at most 50 MB.
pub const MAX_SKEW: u32 = 20_000;

/// A diagonal run is kept when its weight is **greater** than this. spec: ALIGN-1.4.
pub const MIN_RUN_WEIGHT: i64 = 500;
/// A run that reaches this weight is taken at once. spec: ALIGN-1.4.
pub const BIG_RUN_WEIGHT: i64 = 5000;
/// A big run is emitted in pieces: a piece goes out when its count exceeds this, so a piece
/// has `RUN_PIECE_EMIT_ABOVE + 1` = 501 lines and the remainder comes last.
/// spec: ALIGN-1.4, fix: B1.
pub const RUN_PIECE_EMIT_ABOVE: u32 = 500;

/// Orphan lines are emitted in chunks of this many in the Standard aligner. spec: ALIGN-1.4.
pub const ORPHAN_CHUNK_STANDARD: u32 = 200;
/// ... and in the Patience aligner. spec: ALIGN-1.8.
pub const ORPHAN_CHUNK_PATIENCE: u32 = 2000;

/// Score of one unpaired line in the gap DP. spec: ALIGN-1.5.
pub const GAP_PENALTY: i32 = -20;

/// Closeness of two equal-hash lines when a comparable text is empty. spec: ALIGN-1.6.
pub const CLOSENESS_EQUAL_EMPTY: i32 = -10;
/// Closeness with `closeness_same_only` when a side is empty. spec: ALIGN-1.6.
pub const CLOSENESS_SAME_ONLY_EMPTY: i32 = -18;
/// Lowest closeness of two different lines. spec: ALIGN-1.6.
pub const CLOSENESS_FLOOR: i32 = -40;
/// Highest closeness when one side is empty. spec: ALIGN-1.6.
pub const CLOSENESS_EMPTY_CEILING: i32 = -11;
/// Scale of a positive closeness: `s * 25 / min(la, lb)`. spec: ALIGN-1.6.
pub const CLOSENESS_POSITIVE_SCALE: i64 = 25;
/// Score of different lines when closeness is off. spec: ALIGN-1.6.
pub const NO_CLOSENESS_SCORE: i32 = -18;
/// ... and with `never_align_diffs`. spec: ALIGN-1.6.
pub const NEVER_ALIGN_SCORE: i32 = -5000;
/// Upper bound of the closeness text ring cache per side. spec: ALIGN-1.6.
pub const CLOSENESS_CACHE_MAX: u32 = 4096;

/// Cell cap of one gap DP; larger gaps are split along the diagonal. Our own safety limit
/// (Delphi `cDcMaxGapCells`), plan.md §5.2.
pub const MAX_GAP_CELLS: u64 = 16 * 1024 * 1024;
/// Block size when a gap is split. plan.md §5.2.
pub const GAP_SPLIT: u32 = 2000;

/// Size of the closeness ring cache for a skew tolerance: the next power of two that is at
/// least `skew` (and at least 2), capped at [`CLOSENESS_CACHE_MAX`]. spec: ALIGN-1.6.
pub const fn closeness_cache_size(skew: u32) -> usize {
    if skew >= CLOSENESS_CACHE_MAX {
        CLOSENESS_CACHE_MAX as usize
    } else {
        let mut size = 2usize;
        while size < skew as usize {
            size *= 2;
        }
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_size_is_next_power_of_two_capped_at_4096() {
        assert_eq!(closeness_cache_size(1), 2);
        assert_eq!(closeness_cache_size(2), 2);
        assert_eq!(closeness_cache_size(3), 4);
        assert_eq!(closeness_cache_size(2000), 2048);
        assert_eq!(closeness_cache_size(2048), 2048);
        assert_eq!(closeness_cache_size(2049), 4096);
        assert_eq!(closeness_cache_size(4096), 4096);
        assert_eq!(closeness_cache_size(1_000_000), 4096);
    }

    #[test]
    fn appendix_a_values() {
        assert_eq!(DEFAULT_SKEW, 2000);
        assert_eq!(
            (
                WEIGHT_EMPTY,
                DEFAULT_WEIGHT_INSIG + 30,
                DEFAULT_WEIGHT_SIG * 2 + 50
            ),
            (2, 31, 70)
        );
        assert_eq!(LINE_GROWTH_STEP, 128);
        assert_eq!(
            (MIN_RUN_WEIGHT, BIG_RUN_WEIGHT, RUN_PIECE_EMIT_ABOVE),
            (500, 5000, 500)
        );
        assert_eq!((ORPHAN_CHUNK_STANDARD, ORPHAN_CHUNK_PATIENCE), (200, 2000));
        assert_eq!(GAP_PENALTY, -20);
        assert_eq!((CLOSENESS_FLOOR, CLOSENESS_EMPTY_CEILING), (-40, -11));
        assert_eq!((NO_CLOSENESS_SCORE, NEVER_ALIGN_SCORE), (-18, -5000));
        assert_eq!((MAX_GAP_CELLS, GAP_SPLIT), (16 * 1024 * 1024, 2000));
    }
}
