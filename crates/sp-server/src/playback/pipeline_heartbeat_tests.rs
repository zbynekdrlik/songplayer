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

/// #221: a playlist has no NDI output of its own (lane 3), so a bad poll is
/// an underrun or a stale delivery only: a full-rate grid with a fresh
/// delivery is good. Before B4 step 6 every poll at 0 receivers (an output cg
/// OBS no longer showed) counted as bad.
#[test]
fn a_full_rate_poll_with_a_fresh_delivery_is_good() {
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        30.0,
        30.0,
        Some(now),
        now,
    ));
}

#[test]
fn classify_bad_poll_paused_is_never_bad() {
    // Even with fps=0 and no recent submit, the Paused
    // state must not bump consecutive_bad_polls. Same non-Playing guard
    // as Idle / WaitingForScene.
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Paused,
        0.0,
        30.0,
        None,
        Instant::now(),
    ));
}

#[test]
fn classify_bad_poll_idle_is_never_bad() {
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Idle,
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
        10.0,
        30.0,
        Some(Instant::now()),
        Instant::now(),
    ));
    // 16 >= 15 => not bad
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        16.0,
        30.0,
        Some(Instant::now()),
        Instant::now(),
    ));
}

#[test]
fn classify_bad_poll_stale_when_last_submit_more_than_10s_ago() {
    let now = Instant::now();
    // 11s ago, fps healthy => stale bad-poll
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        30.0,
        30.0,
        Some(now - Duration::from_secs(11)),
        now,
    ));
    // 9s ago => not stale, all healthy => not bad
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        30.0,
        30.0,
        Some(now - Duration::from_secs(9)),
        now,
    ));
}

#[test]
fn classify_bad_poll_does_not_trigger_underrun_when_nominal_fps_is_zero() {
    // Kills the `nominal_fps > 0.0` -> `>=` mutant: with nominal_fps=0.0,
    // the guard must skip the underrun branch entirely (otherwise division
    // by zero or always-bad poll). Only the staleness branch can trip in
    // this case, and we provide a fresh last_submit so it doesn't.
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        10.0, // observed
        0.0,  // nominal — guard should skip underrun
        Some(now),
        now,
    ));
}

#[test]
fn classify_bad_poll_underrun_excludes_exact_half_nominal() {
    // Kills the `observed_fps < nominal_fps / 2.0` -> `<=` mutant.
    // observed_fps == nominal/2 must NOT be a bad poll (the threshold
    // is strictly less-than, by spec).
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        15.0, // exactly nominal/2 with nominal=30
        30.0,
        Some(now),
        now,
    ));
    // Just under should still be bad.
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        14.99,
        30.0,
        Some(now),
        now,
    ));
}

#[test]
fn classify_bad_poll_stale_excludes_exact_10s() {
    // Kills the `now.duration_since(ts) > Duration::from_secs(10)`
    // -> `>=` mutant. last_submit_ts exactly 10s ago must NOT be
    // stale (the threshold is strictly greater-than, by spec).
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        30.0,
        30.0,
        Some(now - Duration::from_secs(10)),
        now,
    ));
    // 10s + 1ns should be stale.
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        30.0,
        30.0,
        Some(now - Duration::from_secs(10) - Duration::from_nanos(1)),
        now,
    ));
}
