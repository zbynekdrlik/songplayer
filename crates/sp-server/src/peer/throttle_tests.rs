//! #229 `peer::throttle`: the rate math of the hasher's reads and the peer
//! API's uploads.

use std::time::Duration;

use super::*;

#[test]
fn the_wait_keeps_the_average_at_the_rate() {
    let ms = Duration::from_millis;
    assert_eq!(wait_for(0, ms(0), 1_000), ms(0));
    assert_eq!(wait_for(1_000, ms(0), 1_000), ms(1_000));
    assert_eq!(wait_for(250, ms(0), 1_000), ms(250));
    assert_eq!(wait_for(1_000, ms(400), 1_000), ms(600));
    assert_eq!(wait_for(1_000, ms(1_000), 1_000), ms(0));
    assert_eq!(
        wait_for(1_000, ms(2_000), 1_000),
        ms(0),
        "behind never waits"
    );
    assert_eq!(wait_for(3, ms(0), 2), ms(1_500));
}

/// Rate 0 is no limit. A count far ahead of the rate does not overflow and
/// pauses at most 2 s at a time: a stall, or a wrong rate, never hangs a
/// transfer (or a test) for longer.
#[test]
fn rate_zero_is_no_limit_and_a_pause_is_at_most_two_seconds() {
    let ms = Duration::from_millis;
    assert_eq!(wait_for(u64::MAX, Duration::ZERO, 0), Duration::ZERO);
    assert_eq!(wait_for(u64::MAX, Duration::ZERO, 1), ms(2_000));
    assert_eq!(wait_for(2_500, ms(0), 1_000), ms(2_000), "2.5 s due");
    assert_eq!(wait_for(2_500, ms(600), 1_000), ms(1_900));
}

#[test]
fn mbit_per_second_in_bytes() {
    assert_eq!(mbps_to_bytes(1), 125_000);
    assert_eq!(mbps_to_bytes(20), 2_500_000);
    assert_eq!(mbps_to_bytes(10_000), 1_250_000_000);
}
