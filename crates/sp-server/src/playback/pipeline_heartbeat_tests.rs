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
fn classify_bad_poll_paused_is_never_bad() {
    // Even with connections=0, fps=0, and no recent submit, the Paused
    // state must not bump consecutive_bad_polls. Same non-Playing guard
    // as Idle / WaitingForScene.
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Paused,
        0,
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

#[test]
fn classify_bad_poll_does_not_trigger_underrun_when_nominal_fps_is_zero() {
    // Kills the `nominal_fps > 0.0` -> `>=` mutant: with nominal_fps=0.0,
    // the guard must skip the underrun branch entirely (otherwise division
    // by zero or always-bad poll). Only the staleness branch can trip in
    // this case, and we provide a fresh last_submit so it doesn't.
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
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
        1,
        15.0, // exactly nominal/2 with nominal=30
        30.0,
        Some(now),
        now,
    ));
    // Just under should still be bad.
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        14.99,
        30.0,
        Some(now),
        now,
    ));
}

// ---------------------------------------------------------------------------
// Regression test for #133: /api/v1/ndi/health froze on the last pre-pause
// snapshot (state=Playing, stale fps) because decode_and_send's paused
// branch never emitted a HealthSnapshot event at all. This proves the
// paused-tick heartbeat reports Paused with zeroed fps and a non-increasing
// frame total instead — exercised via MockNdiBackend since decode_and_send
// itself is Windows-only and cannot run on Linux CI (no MediaFoundation),
// but emit_heartbeat only needs a generic FrameSubmitter<B: NdiBackend>,
// same as submitter.rs's own tests.
// ---------------------------------------------------------------------------
#[test]
fn paused_heartbeat_reports_paused_state_with_non_increasing_counters() {
    use crate::playback::submitter::FrameSubmitter;
    use sp_ndi::test_util::MockNdiBackend;
    use std::sync::Arc;

    let backend = Arc::new(MockNdiBackend::new());
    let sender = sp_ndi::NdiSender::new_with_clocking(backend, "SP-test", true, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);

    // Simulate active playback: 5 real frames submitted before pause.
    for _ in 0..5 {
        submitter.submit_nv12(4, 2, 4, vec![0u8; 4 * 2 * 3 / 2], &[]);
    }
    let total_before_pause = submitter.frames_submitted_total();
    assert_eq!(total_before_pause, 5);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Force should_run_heartbeat(...) to gate true on the first tick.
    let mut last_heartbeat = Instant::now() - Duration::from_secs(6);
    let mut consecutive_bad_polls: u32 = 0;

    // Paused tick: no new frames submitted since pause began.
    run_heartbeat_paused(
        &mut submitter,
        &event_tx,
        42,
        &mut last_heartbeat,
        &mut consecutive_bad_polls,
    );

    let (playlist_id, event) = event_rx.try_recv().expect(
        "paused branch must emit a HealthSnapshot event — a silent pause \
         freezes /api/v1/ndi/health on the pre-pause snapshot forever",
    );
    assert_eq!(playlist_id, 42);
    match event {
        PipelineEvent::HealthSnapshot {
            reported_state,
            observed_fps,
            frames_submitted_total,
            ..
        } => {
            assert_eq!(
                reported_state,
                PlaybackStateLabel::Paused,
                "must report Paused, never the stale Playing state"
            );
            assert_eq!(
                observed_fps, 0.0,
                "no frames submitted while paused -> fps must be zeroed"
            );
            assert_eq!(
                frames_submitted_total, total_before_pause,
                "frame total must not increase while paused"
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

/// A paused tick BEFORE the 5s cadence has elapsed must not emit anything —
/// matches the Playing branch's should_run_heartbeat gate so pausing doesn't
/// spam the health channel every 100ms poll.
#[test]
fn paused_heartbeat_respects_5s_cadence() {
    use crate::playback::submitter::FrameSubmitter;
    use sp_ndi::test_util::MockNdiBackend;
    use std::sync::Arc;

    let backend = Arc::new(MockNdiBackend::new());
    let sender = sp_ndi::NdiSender::new_with_clocking(backend, "SP-test2", true, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut last_heartbeat = Instant::now(); // just ticked — well under 5s
    let mut consecutive_bad_polls: u32 = 0;

    run_heartbeat_paused(
        &mut submitter,
        &event_tx,
        7,
        &mut last_heartbeat,
        &mut consecutive_bad_polls,
    );

    assert!(
        event_rx.try_recv().is_err(),
        "must not emit before the 5s heartbeat cadence elapses"
    );
}

#[test]
fn classify_bad_poll_stale_excludes_exact_10s() {
    // Kills the `now.duration_since(ts) > Duration::from_secs(10)`
    // -> `>=` mutant. last_submit_ts exactly 10s ago must NOT be
    // stale (the threshold is strictly greater-than, by spec).
    let now = Instant::now();
    assert!(!classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        30.0,
        30.0,
        Some(now - Duration::from_secs(10)),
        now,
    ));
    // 10s + 1ns should be stale.
    assert!(classify_bad_poll(
        &PlaybackStateLabel::Playing,
        1,
        30.0,
        30.0,
        Some(now - Duration::from_secs(10) - Duration::from_nanos(1)),
        now,
    ));
}
