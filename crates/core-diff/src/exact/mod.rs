//! Exact-match engines over hashed lines (plan.md §5.3, decision D8): Myers (minimal),
//! Histogram, and Patience anchors. They work on `u32` hashes, never touch the file system,
//! and are validated against a brute-force LCS and `imara-diff` (dev-dependency only).

pub mod histogram;
pub mod myers;
pub mod patience;
pub mod prefilter;

pub use histogram::{HistogramScratch, MAX_CHAIN, histogram_matches, histogram_matches_into};
pub use myers::{MyersScratch, myers_matches, myers_matches_into, myers_range_into};
pub use patience::{PatienceScratch, patience_anchors};
pub use prefilter::Prefilter;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A multiplicative hasher for keys that are already CRC hashes. SipHash (the std default) is
/// several times slower for the millions of lookups the histogram and patience engines make.
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_u32(&mut self, x: u32) {
        self.0 = u64::from(x).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// `HashMap` with [`IdHasher`].
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;

pub(crate) fn fast_map<K, V>() -> FastMap<K, V> {
    HashMap::default()
}

impl std::fmt::Debug for IdHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdHasher")
    }
}
