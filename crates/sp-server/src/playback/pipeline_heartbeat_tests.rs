//! Unit tests for the pure pipeline heartbeat helpers.
//! Included into `pipeline.rs` via `#[path = "pipeline_heartbeat_tests.rs"]`
//! so that `super::*` resolves to `pipeline`'s private items.

use super::*;
use crate::playback::ndi_health::PlaybackStateLabel;
use std::time::{Duration, Instant};

#[test]
fn should_run_heartbeat_returns_true_on_or_after_5_seconds() {
    assert!(should_run_heartbeat(Duration::from_secs(5)));
    assert!(should_run_heartbeat(Duration::from_secs(6)));
    assert!(should_run_heartbeat(Duration::from_millis(10_000)));
}

#[test]
fn should_run_heartbeat_returns_false_below_5_seconds() {
    assert!(!should_run_heartbeat(Duration::from_secs(0)));
    assert!(!should_run_heartbeat(Duration::from_secs(4)));
    assert!(!should_run_heartbeat(Duration::from_millis(4_999)));
}

#[test]
fn classify_bad_poll_connections_zero_while_playing() {
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        0,
        30.0,
        30.0,
        None,
        Instant::now(),
    ));
}

#[test]
fn classify_bad_poll_idle_is_never_bad() {
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Idle,
        0,
        0.0,
        30.0,
        None,
        Instant::now(),
    ));
}

#[test]
fn classify_bad_poll_underrun_when_observed_below_half_nominal() {
    // 10 < 30/2=15 => bad
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        10.0,
        30.0,
        Some(Instant::now()),
        Instant::now(),
    ));
    // 16 >= 15 => not bad
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        16.0,
        30.0,
        Some(Instant::now()),
        Instant::now(),
    ));
}

#[test]
fn classify_bad_poll_stale_when_last_submit_more_than_10s_ago() {
    let now = Instant::now();
    // 11s ago, fps healthy, connections healthy => stale bad-poll
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        30.0,
        30.0,
        Some(now - Duration::from_secs(11)),
        now,
    ));
    // 9s ago => not stale, all healthy => not bad
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        30.0,
        30.0,
        Some(now - Duration::from_secs(9)),
        now,
    ));
}
