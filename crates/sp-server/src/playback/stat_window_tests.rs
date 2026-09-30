//! #210 part 2: the shared window helpers, exact pins.
//! Wired via `#[cfg(test)] #[path = "stat_window_tests.rs"] mod tests;`.

use super::*;

#[test]
fn a_u64_s_worst_is_its_max() {
    assert_eq!(7_u64.worst(3), 7);
    assert_eq!(3_u64.worst(7), 7);
}

#[test]
fn two_buckets_keep_the_worst_of_the_bucket_being_filled_and_the_last_full_one() {
    let mut w = TwoBucketWorst::<u64>::default();
    assert_eq!(w.worst(), 0, "nothing yet");
    w.push(9, 3);
    w.push(1, 3);
    assert_eq!(w.worst(), 9, "2 of 3: one bucket");
    w.push(1, 3);
    assert_eq!(w.worst(), 9, "3: the full bucket is the last one");
    w.push(2, 3);
    w.push(2, 3);
    assert_eq!(w.worst(), 9, "5: still the last full bucket");
    w.push(2, 3);
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
