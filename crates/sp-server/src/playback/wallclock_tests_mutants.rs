//! Mutation-killing unit tests for [`utc_now_100ns`] (#153, PR #153).
//!
//! `utc_now_100ns` returns `Utc::now().timestamp_nanos_opt().map(|ns| ns / 100)
//! .unwrap_or(0)` — the real wall time in 100-ns units since the Unix epoch, an
//! astronomically large number. That single fact kills every listed mutant:
//! the whole-function constant replacements (`0`, `1`, `-1`) and the
//! `ns / 100 -> ns % 100` mutant (a remainder is only 0..=99) all fall far
//! below a year-2020 threshold.
//!
//! Wired via `#[cfg(test)] #[path = "wallclock_tests_mutants.rs"]` at the bottom
//! of `wallclock.rs`. `utc_now_100ns` is a module-level `pub fn`, called directly.

use super::utc_now_100ns;

#[test]
fn utc_now_100ns_is_the_real_epoch_time_in_100ns_units() {
    let v = utc_now_100ns();
    // Year 2020 in 100-ns-since-epoch units is 16_000_000_000_000_000; "now" is
    // well past it. One lower bound kills every listed mutant:
    //   -> 0, -> 1, -> -1     (all far below the threshold), and
    //   ns / 100 -> ns % 100  (a remainder is only 0..=99).
    assert!(
        v > 16_000_000_000_000_000,
        "utc_now_100ns must be the real epoch time in 100-ns units, got {v}"
    );
    // Upper sanity bound (~year 2287) so no wildly-scaled value passes silently.
    assert!(
        v < 100_000_000_000_000_000,
        "utc_now_100ns must be a plausible current wall time, got {v}"
    );
}
