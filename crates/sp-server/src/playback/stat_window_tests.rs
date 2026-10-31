//! #210 part 2: the shared window helpers, exact pins.
//! Wired via `#[cfg(test)] #[path = "stat_window_tests.rs"] mod tests;`.

use std::cell::Cell;

use super::*;

#[test]
fn a_u64_s_worst_is_its_max() {
    assert_eq!(7_u64.worst(3), 7);
    assert_eq!(3_u64.worst(7), 7);
}

#[test]
fn two_buckets_keep_the_worst_of_the_bucket_being_filled_and_the_last_full_one() {
    let mut w = TwoBucketWorst::<u64, 3>::default();
    assert_eq!(w.worst(), 0, "nothing yet");
    w.push(9);
    w.push(1);
    assert_eq!(w.worst(), 9, "2 of 3: one bucket");
    w.push(1);
    assert_eq!(w.worst(), 9, "3: the full bucket is the last one");
    w.push(2);
    w.push(2);
    assert_eq!(w.worst(), 9, "5: still the last full bucket");
    w.push(2);
    assert_eq!(w.worst(), 2, "6: the 9 is two buckets back — gone");
}

#[test]
fn the_limiter_admits_one_per_period_and_counts_the_ones_it_held_back() {
    let mut l = WarnLimiter::default();
    assert_eq!(l.admit(1_000, 50), Some(0), "the first one");
    assert_eq!(l.admit(1_020, 50), None, "inside the period");
    assert_eq!(l.admit(1_049, 50), None, "one short of the period");
    assert_eq!(
        l.admit(1_050, 50),
        Some(2),
        "a period after the last WARN: with the two it held back"
    );
    assert_eq!(l.admit(1_060, 50), None);
    assert_eq!(
        l.admit(1_200, 50),
        Some(1),
        "the count restarted at the last WARN"
    );
}

/// #279 P1b review round 3: a state's changes for the log, one line per
/// change, at most one per period; a change held back is said after the
/// window as the state is then; the state is built only for a line.
#[test]
fn a_state_log_says_each_change_once_and_builds_the_state_only_for_a_line() {
    let built = Cell::new(0);
    let build = |s: &str| {
        built.set(built.get() + 1);
        Some(s.to_owned())
    };
    let mut log = StateLog::<Option<String>>::default();
    assert_eq!(log.change(0, 10, false, || build("x")), None, "no change");
    assert_eq!(built.get(), 0, "nothing built");
    assert_eq!(log.change(1, 10, true, || build("a")), Some(0));
    assert_eq!(log.logged().as_deref(), Some("a"));
    assert_eq!(
        log.change(5, 10, true, || build("b")),
        None,
        "in the window"
    );
    assert_eq!(built.get(), 1, "a change held back builds nothing");
    assert_eq!(log.change(11, 10, true, || build("b")), Some(1));
    assert_eq!(log.logged().as_deref(), Some("b"));
    assert_eq!(built.get(), 2);
}
