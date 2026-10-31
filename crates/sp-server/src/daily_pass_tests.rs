//! The daily pass's timer (the #260 retention, the #256 usage-log prune).

use std::time::Duration;

use super::next_wait;

#[test]
fn a_pass_a_day_and_an_hour_after_a_held_one() {
    assert_eq!(next_wait(false), Duration::from_secs(86_400));
    assert_eq!(next_wait(true), Duration::from_secs(3_600));
}
