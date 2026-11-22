//! Alignment settings and their validation (plan.md §5.1, spec ALIGN-1.1).

use core_base::{Error, Result};

use super::consts::DEFAULT_SKEW;

/// Which alignment engine runs. The numeric values follow the original's enum order and are
/// fixed (they cross the C ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum AlignKind {
    /// "Unaligned": common prefix, then lines paired in order (the Standard aligner with
    /// skew 1 and closeness off).
    None = 0,
    Patience = 1,
    Myers = 2,
    /// "Standard alignment", the default.
    #[default]
    Standard = 3,
    /// Not in the original (decision D12).
    Histogram = 4,
}

/// User-facing alignment settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignConfig {
    pub kind: AlignKind,
    /// Window size in lines per side; must be at least 1.
    pub skew_tolerance: u32,
    /// `None` = the default of the kind: on for every kind except Myers.
    pub use_closeness: Option<bool>,
    pub closeness_same_only: bool,
    /// Never pair different lines. Cannot be combined with closeness.
    pub never_align_diffs: bool,
    /// Orphan lines are always important, even blank ones.
    pub orphans_always_sig: bool,
}

impl Default for AlignConfig {
    fn default() -> Self {
        AlignConfig {
            kind: AlignKind::Standard,
            skew_tolerance: DEFAULT_SKEW,
            use_closeness: None,
            closeness_same_only: false,
            never_align_diffs: false,
            orphans_always_sig: false,
        }
    }
}

/// The settings the aligner actually runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// Never `AlignKind::None`: that runs as Standard with skew 1 and closeness off.
    pub kind: AlignKind,
    pub skew: u32,
    pub use_closeness: bool,
    pub closeness_same_only: bool,
    pub never_align_diffs: bool,
    pub orphans_always_sig: bool,
}

impl AlignConfig {
    /// Whether closeness is on for the configured kind (`None` = on except for Myers).
    pub fn effective_use_closeness(&self) -> bool {
        self.use_closeness.unwrap_or(self.kind != AlignKind::Myers)
    }

    /// Validates the settings and resolves them.
    ///
    /// spec: ALIGN-1.1 (`TBcAlignLines.Run`): `never_align_diffs` and closeness cannot both be
    /// on, and the skew tolerance must be at least 1. `AlignKind::None` then runs as Standard
    /// with skew 1 and closeness off (`TBcSmartAlign.Run`).
    pub fn resolve(&self) -> Result<Resolved> {
        if self.never_align_diffs && self.effective_use_closeness() {
            return Err(Error::invalid_argument(
                "never_align_diffs and use_closeness cannot both be on",
            ));
        }
        if self.skew_tolerance < 1 {
            return Err(Error::invalid_argument("skew_tolerance must be at least 1"));
        }
        // decision: D41: the window bitset grows with the square of the skew
        if self.skew_tolerance > super::consts::MAX_SKEW {
            return Err(Error::invalid_argument(format!(
                "skew_tolerance must be at most {}",
                super::consts::MAX_SKEW
            )));
        }
        let (kind, skew, closeness) = if self.kind == AlignKind::None {
            (AlignKind::Standard, 1, false)
        } else {
            (
                self.kind,
                self.skew_tolerance,
                self.effective_use_closeness() && !self.never_align_diffs,
            )
        };
        Ok(Resolved {
            kind,
            skew,
            use_closeness: closeness,
            closeness_same_only: closeness && self.closeness_same_only,
            never_align_diffs: self.never_align_diffs,
            orphans_always_sig: self.orphans_always_sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_base::ErrorKind;

    #[test]
    fn defaults_match_the_spec() {
        let c = AlignConfig::default();
        assert_eq!(c.kind, AlignKind::Standard);
        assert_eq!(c.skew_tolerance, 2000);
        assert_eq!(c.use_closeness, None);
        assert!(!c.closeness_same_only && !c.never_align_diffs && !c.orphans_always_sig);
        let r = c.resolve().unwrap();
        assert!(r.use_closeness);
        assert_eq!((r.kind, r.skew), (AlignKind::Standard, 2000));
    }

    #[test]
    fn closeness_default_is_on_except_for_myers() {
        for (kind, want) in [
            (AlignKind::Standard, true),
            (AlignKind::Patience, true),
            (AlignKind::Histogram, true),
            (AlignKind::None, true),
            (AlignKind::Myers, false),
        ] {
            let c = AlignConfig {
                kind,
                ..AlignConfig::default()
            };
            assert_eq!(c.effective_use_closeness(), want, "{kind:?}");
        }
        // an explicit setting wins in both directions
        let c = AlignConfig {
            kind: AlignKind::Myers,
            use_closeness: Some(true),
            ..Default::default()
        };
        assert!(c.resolve().unwrap().use_closeness);
        let c = AlignConfig {
            use_closeness: Some(false),
            ..Default::default()
        };
        assert!(!c.resolve().unwrap().use_closeness);
    }

    #[test]
    fn never_align_diffs_with_closeness_is_an_error() {
        let c = AlignConfig {
            never_align_diffs: true,
            ..Default::default()
        };
        let err = c.resolve().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
        // fine once closeness is switched off, or for Myers whose default is off
        let ok = AlignConfig {
            never_align_diffs: true,
            use_closeness: Some(false),
            ..Default::default()
        };
        let r = ok.resolve().unwrap();
        assert!(r.never_align_diffs && !r.use_closeness);
        let myers = AlignConfig {
            kind: AlignKind::Myers,
            never_align_diffs: true,
            ..Default::default()
        };
        assert!(myers.resolve().is_ok());
    }

    #[test]
    fn skew_above_the_maximum_is_an_error_and_the_maximum_is_accepted() {
        // decision: D41
        let at = AlignConfig {
            skew_tolerance: super::super::consts::MAX_SKEW,
            ..AlignConfig::default()
        };
        assert!(at.resolve().is_ok());
        let over = AlignConfig {
            skew_tolerance: super::super::consts::MAX_SKEW + 1,
            ..AlignConfig::default()
        };
        assert_eq!(
            over.resolve().unwrap_err().kind(),
            core_base::ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn skew_below_one_is_an_error() {
        let c = AlignConfig {
            skew_tolerance: 0,
            ..Default::default()
        };
        assert_eq!(c.resolve().unwrap_err().kind(), ErrorKind::InvalidArgument);
        assert!(
            AlignConfig {
                skew_tolerance: 1,
                ..Default::default()
            }
            .resolve()
            .is_ok()
        );
    }

    #[test]
    fn none_resolves_to_standard_with_skew_one_and_no_closeness() {
        let c = AlignConfig {
            kind: AlignKind::None,
            closeness_same_only: true,
            ..Default::default()
        };
        let r = c.resolve().unwrap();
        assert_eq!(
            (r.kind, r.skew, r.use_closeness),
            (AlignKind::Standard, 1, false)
        );
        assert!(
            !r.closeness_same_only,
            "same-only is meaningless without closeness"
        );
    }

    #[test]
    fn same_only_needs_closeness() {
        let c = AlignConfig {
            closeness_same_only: true,
            use_closeness: Some(false),
            ..Default::default()
        };
        assert!(!c.resolve().unwrap().closeness_same_only);
        let c = AlignConfig {
            closeness_same_only: true,
            ..Default::default()
        };
        assert!(c.resolve().unwrap().closeness_same_only);
    }

    #[test]
    fn kind_numbers_are_stable() {
        assert_eq!(
            [
                AlignKind::None,
                AlignKind::Patience,
                AlignKind::Myers,
                AlignKind::Standard,
                AlignKind::Histogram
            ]
            .map(|k| k as u32),
            [0, 1, 2, 3, 4]
        );
    }
}
