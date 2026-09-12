//! Genlock stamping tests for [`FrameSubmitter`] (#146). A fake, frozen
//! [`WallClock`] makes the stamped timecodes deterministic without any
//! `sleep`. Wired via
//! `#[cfg(test)] #[path = "submitter_tests_timecode.rs"] mod submitter_tests_timecode;`.

use super::*;
use crate::playback::wallclock::WallClock;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use std::sync::Arc;

fn audio(tc_ignored: Option<i64>) -> Vec<AudioFrame> {
    vec![AudioFrame {
        data: vec![0.1, 0.2, 0.3, 0.4],
        channels: 2,
        sample_rate: 48000,
        // The submitter re-stamps audio at submission; this incoming value
        // is deliberately whatever — the test asserts the submitter's stamp.
        timecode_100ns: tc_ignored,
    }]
}

#[test]
fn video_timecode_is_floored_to_grid_and_audio_is_raw_wall() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "G", true, false).unwrap();
    let fake_now: i64 = 123_456_789;
    let mut sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, WallClock::fixed(fake_now));

    sub.submit_nv12(4, 2, 4, vec![0u8; 12], &audio(None));

    // Video: floored to the 30 fps grid (contract §4 — FLOOR, never ceil).
    let expected_video = floor_boundary_100ns(fake_now, GENLOCK_GRID_FPS);
    assert_eq!(
        backend.video_timecodes(),
        vec![expected_video],
        "video timecode must be the floored 30 fps boundary"
    );
    // Audio: raw wall clock at submission, NO boundary snap (contract §6).
    assert_eq!(
        backend.audio_timecodes(),
        vec![fake_now],
        "audio timecode must be the raw wall clock, unsnapped"
    );
    // The fixture is deliberately off-grid so the snap is observable.
    assert_ne!(
        expected_video, fake_now,
        "fixture must be off-grid so the floor is observable"
    );
}

#[test]
fn audio_is_submitted_before_video() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "G2", true, false).unwrap();
    let mut sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, WallClock::fixed(1_000_000));

    sub.submit_nv12(4, 2, 4, vec![0u8; 12], &audio(None));

    let calls = backend.calls();
    let idx_audio = calls
        .iter()
        .position(|c| c.starts_with("send_audio"))
        .expect("audio must be sent");
    let idx_video = calls
        .iter()
        .position(|c| c.starts_with("send_video_async"))
        .expect("video must be sent");
    assert!(
        idx_audio < idx_video,
        "audio must be submitted before video: {calls:#?}"
    );
}
