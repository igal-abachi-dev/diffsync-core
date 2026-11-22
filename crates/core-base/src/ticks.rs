//! FILETIME ticks: 100 ns units since 1601-01-01 UTC, as in `Dc.Vfs` (plan.md §3).
//!
//! Folder compare, the VFS and the sync log all keep times in this one unit so Windows
//! `FILETIME` values pass through unchanged and the tolerance rules (2 s, 3600 s, quarter
//! hours) are plain integer arithmetic.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 100 ns since 1601-01-01 UTC. Negative values are dates before 1601 and are legal.
pub type Ticks = i64;

pub const TICKS_PER_MILLI: i64 = 10_000;
pub const TICKS_PER_SECOND: i64 = 10_000_000;
/// Ticks between 1601-01-01 and 1970-01-01.
pub const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// Whole seconds as ticks (saturating).
pub const fn ticks_from_secs(secs: i64) -> Ticks {
    secs.saturating_mul(TICKS_PER_SECOND)
}

/// Combines the two halves of a Win32 `FILETIME` (`dwLowDateTime`, `dwHighDateTime`).
/// FILETIME is unsigned; values above `i64::MAX` do not occur for real dates.
pub const fn ticks_from_filetime(low: u32, high: u32) -> Ticks {
    (((high as u64) << 32) | low as u64) as i64
}

/// Splits ticks back into `(low, high)` for a `FILETIME`.
pub const fn ticks_to_filetime(t: Ticks) -> (u32, u32) {
    let v = t as u64;
    (v as u32, (v >> 32) as u32)
}

/// Converts a `SystemTime` to ticks. Sub-100 ns parts are floored, which keeps pre-1970 and
/// post-1970 times consistent. Out-of-range times saturate.
pub fn ticks_from_system_time(t: SystemTime) -> Ticks {
    let ticks: i128 = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i128::from(UNIX_EPOCH_TICKS) + (d.as_nanos() / 100) as i128,
        Err(e) => {
            // Before 1970: floor the magnitude division the other way.
            let nanos = e.duration().as_nanos();
            i128::from(UNIX_EPOCH_TICKS) - nanos.div_ceil(100) as i128
        }
    };
    ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Converts ticks to a `SystemTime`, or `None` when the platform cannot represent it
/// (for example dates before 1601 on Windows).
pub fn ticks_to_system_time(t: Ticks) -> Option<SystemTime> {
    let diff = i128::from(t) - i128::from(UNIX_EPOCH_TICKS);
    let nanos = diff.unsigned_abs() * 100;
    let secs = u64::try_from(nanos / 1_000_000_000).ok()?;
    let sub = (nanos % 1_000_000_000) as u32;
    let d = Duration::new(secs, sub);
    if diff >= 0 {
        UNIX_EPOCH.checked_add(d)
    } else {
        UNIX_EPOCH.checked_sub(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_is_the_documented_constant() {
        assert_eq!(ticks_from_system_time(UNIX_EPOCH), UNIX_EPOCH_TICKS);
        assert_eq!(ticks_to_system_time(UNIX_EPOCH_TICKS), Some(UNIX_EPOCH));
        // 1601-01-01 is tick 0 (FILETIME zero).
        assert_eq!(UNIX_EPOCH_TICKS / TICKS_PER_SECOND, 11_644_473_600);
    }

    #[test]
    fn round_trip_modern_dates() {
        for secs in [0u64, 1, 86_400, 1_000_000_000, 1_700_000_000, 4_102_444_800] {
            let st = UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(1_234_500);
            let t = ticks_from_system_time(st);
            assert_eq!(ticks_to_system_time(t), Some(st), "secs {secs}");
        }
    }

    #[test]
    fn round_trip_pre_1970() {
        for secs in [1u64, 86_400, 1_000_000_000, 2_000_000_000] {
            let st = UNIX_EPOCH - Duration::from_secs(secs) - Duration::from_nanos(700);
            let t = ticks_from_system_time(st);
            assert_eq!(ticks_to_system_time(t), Some(st), "secs {secs}");
            assert!(t < UNIX_EPOCH_TICKS);
        }
    }

    #[test]
    fn whole_ticks_around_the_epoch() {
        let st = UNIX_EPOCH - Duration::from_nanos(200);
        assert_eq!(ticks_from_system_time(st), UNIX_EPOCH_TICKS - 2);
        let st = UNIX_EPOCH + Duration::from_nanos(200);
        assert_eq!(ticks_from_system_time(st), UNIX_EPOCH_TICKS + 2);
    }

    /// Windows' `SystemTime` already has 100 ns resolution, so sub-tick parts only exist on
    /// platforms with nanosecond clocks.
    #[cfg(not(windows))]
    #[test]
    fn pre_1970_sub_tick_parts_floor() {
        // 150 ns before the epoch is tick -2 (floor), not -1.
        let st = UNIX_EPOCH - Duration::from_nanos(150);
        assert_eq!(ticks_from_system_time(st), UNIX_EPOCH_TICKS - 2);
        // 150 ns after is tick +1 (floor).
        let st = UNIX_EPOCH + Duration::from_nanos(150);
        assert_eq!(ticks_from_system_time(st), UNIX_EPOCH_TICKS + 1);
    }

    #[test]
    fn filetime_zero_is_1601() {
        assert_eq!(ticks_from_filetime(0, 0), 0);
        let (lo, hi) = ticks_to_filetime(UNIX_EPOCH_TICKS);
        assert_eq!(ticks_from_filetime(lo, hi), UNIX_EPOCH_TICKS);
        assert_eq!(ticks_from_filetime(0xFFFF_FFFF, 0x7FFF_FFFF), i64::MAX);
    }

    #[test]
    fn secs_helper_saturates() {
        assert_eq!(ticks_from_secs(2), 20_000_000);
        assert_eq!(ticks_from_secs(i64::MAX), i64::MAX);
        assert_eq!(ticks_from_secs(-3600), -36_000_000_000);
    }

    #[test]
    fn unrepresentable_ticks_return_none_instead_of_panicking() {
        // i64::MIN ticks is year ~-27000. Windows' SystemTime starts at 1601 and says None;
        // Unix platforms can hold it. Either way the call must not panic.
        let r = ticks_to_system_time(i64::MIN);
        if cfg!(windows) {
            assert_eq!(r, None);
        }
    }
}
