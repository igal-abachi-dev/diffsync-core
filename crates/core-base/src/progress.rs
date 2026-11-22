//! Progress reporting types (plan.md §3).

/// What a long-running job is doing. Numeric values are stable: they cross the C ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum Stage {
    #[default]
    Idle = 0,
    Loading = 1,
    Indexing = 2,
    Aligning = 3,
    Scanning = 4,
    Comparing = 5,
    Planning = 6,
    Copying = 7,
    Deleting = 8,
    Verifying = 9,
    Done = 10,
}

/// A progress snapshot: `done` of `total` units for the current stage. `total` is 0 when it
/// is not known yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
}

impl Progress {
    pub const fn new(stage: Stage, done: u64, total: u64) -> Self {
        Progress { stage, done, total }
    }

    /// Fraction in `0.0..=1.0`, or `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| (self.done.min(self.total) as f64) / (self.total as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_is_clamped_and_optional() {
        assert_eq!(Progress::new(Stage::Copying, 5, 0).fraction(), None);
        assert_eq!(Progress::new(Stage::Copying, 5, 10).fraction(), Some(0.5));
        assert_eq!(Progress::new(Stage::Copying, 15, 10).fraction(), Some(1.0));
    }

    #[test]
    fn stage_values_are_stable() {
        assert_eq!(Stage::Idle as u32, 0);
        assert_eq!(Stage::Aligning as u32, 3);
        assert_eq!(Stage::Done as u32, 10);
    }
}
