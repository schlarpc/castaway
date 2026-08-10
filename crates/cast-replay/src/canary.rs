//! The expiry canary's arithmetic — pure over `(horizon, today)`.
//!
//! Each checked-in signature table is a *floor* with a fixed end date (`cks`
//! stops 2027-12-06, `airserver` 2027-03-21). Past it, only the live endpoint can
//! produce a credential; a panel that has lost its uplink simply stops answering
//! official senders, silently, because a table running out is not a runtime error
//! — it is a fact about the calendar the binary was built on.
//!
//! So the warning has to arrive *before* that, as a red check on an ordinary
//! build. This module is the sans-I/O half (ground rule 3): the comparison is a
//! pure function of the table's horizon and the day the test runs. The test in
//! `tests/expiry_canary.rs` reads today once, at its own boundary, and passes it
//! in — no clock is read in here.

/// Seconds in one day. Both horizons and every window bound land on `00:00:00Z`.
pub const SECS_PER_DAY: i64 = 86_400;

/// How many whole days of table a build must still have ahead of it before the
/// canary goes red. A quarter is enough lead to re-export the fixtures and land
/// the change without an emergency, and short enough that it is not perpetually
/// red for a table that is fine.
pub const MIN_REMAINING_DAYS: i64 = 90;

/// Whole days from `now_unix` until `horizon_unix`, floored toward negative
/// infinity — so a horizon already in the past reports a negative number rather
/// than wrapping or saturating to zero.
///
/// Pure over its two arguments; reads no clock. `horizon_unix` is a table's
/// `HORIZON_UNIX` (the first instant it does not cover); `now_unix` is the day the
/// caller decides to measure against, read at the caller's boundary.
#[must_use]
pub const fn days_until(horizon_unix: i64, now_unix: i64) -> i64 {
    (horizon_unix - now_unix).div_euclid(SECS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed reference point so these assertions never read a clock: 2026-08-09,
    // the day the canary was written.
    const REF_2026_08_09: i64 = 1_786_579_200;

    #[test]
    fn days_until_counts_whole_days_ahead() {
        // Exactly 90 days later.
        assert_eq!(
            days_until(REF_2026_08_09 + 90 * SECS_PER_DAY, REF_2026_08_09),
            90
        );
    }

    #[test]
    fn a_partial_day_floors_down_so_the_threshold_is_conservative() {
        // 89 days and 23 hours left is fewer than 90 whole days, and must read as
        // 89 so `>= MIN_REMAINING_DAYS` fails rather than rounding up past it.
        let horizon = REF_2026_08_09 + 90 * SECS_PER_DAY - 3_600;
        assert_eq!(days_until(horizon, REF_2026_08_09), 89);
    }

    #[test]
    fn a_horizon_already_past_is_negative_not_zero() {
        assert_eq!(
            days_until(REF_2026_08_09 - SECS_PER_DAY, REF_2026_08_09),
            -1
        );
    }
}
