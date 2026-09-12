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

/// A [`ClockSource`] that counts realtime samples — proves `new_with_wallclock`
/// wires the injected clock directly (no throwaway `WallClock::system()`, no
/// re-sample on construction) and that the submit read path is monotonic-only
/// (#146 follow-up).
struct SampleCountingClock {
    base: std::time::Instant,
    utc_100ns: i64,
    samples: std::sync::atomic::AtomicU64,
}

impl SampleCountingClock {
    fn new(utc_100ns: i64) -> Arc<Self> {
        Arc::new(Self {
            base: std::time::Instant::now(),
            utc_100ns,
            samples: std::sync::atomic::AtomicU64::new(0),
        })
    }
    fn samples(&self) -> u64 {
        self.samples.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::playback::wallclock::ClockSource for Arc<SampleCountingClock> {
    fn sample(&self) -> (std::time::Instant, i64) {
        self.samples
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.base, self.utc_100ns)
    }
    fn now_monotonic(&self) -> std::time::Instant {
        self.base
    }
}

#[test]
fn new_with_wallclock_uses_the_injected_clock_directly() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "IC", true, false).unwrap();

    let clock = SampleCountingClock::new(7_000_000);
    let wall = WallClock::new(Box::new(clock.clone()));
    // WallClock::new seeded the anchor with exactly one realtime sample.
    assert_eq!(clock.samples(), 1);

    let mut sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, wall);
    // Construction must NOT build a throwaway system clock nor re-sample the
    // injected clock — the injected clock is used directly.
    assert_eq!(
        clock.samples(),
        1,
        "new_with_wallclock must use the injected clock directly"
    );

    // The injected clock is the one that stamps; the hot read path is
    // monotonic-only, so a submit adds no further realtime sample.
    sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
    assert_eq!(
        clock.samples(),
        1,
        "the submit read path must be monotonic-only"
    );
    assert_eq!(
        backend.video_timecodes(),
        vec![floor_boundary_100ns(7_000_000, GENLOCK_GRID_FPS)],
        "video timecode must derive from the injected clock"
    );
}

#[test]
fn paced_black_frame_is_stamped_on_grid_legacy_is_synthesize() {
    // #147 change 6: on the paced path a standby/black frame is a REAL send, so
    // it carries its on-grid boundary (§4.3 — never SYNTHESIZE); the legacy
    // SDK-clocked path keeps SYNTHESIZE (None).
    let fake_now: i64 = 123_456_789; // deliberately off-grid

    // Legacy (paced flag not set): SYNTHESIZE.
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "BL", true, false).unwrap();
    let mut legacy = FrameSubmitter::new_with_wallclock(sender, 30, 1, WallClock::fixed(fake_now));
    legacy.send_black_bgra(1920, 1080);
    assert_eq!(
        backend.video_timecodes(),
        vec![sp_ndi::NDI_SEND_TIMECODE_SYNTHESIZE],
        "legacy standby frame must be SYNTHESIZE"
    );

    // Paced: floored on-grid boundary at the send instant.
    let backend2 = Arc::new(MockNdiBackend::new());
    let sender2 = NdiSender::new_with_clocking(backend2.clone(), "BP", false, false).unwrap();
    let mut paced = FrameSubmitter::new_with_wallclock(sender2, 30, 1, WallClock::fixed(fake_now));
    paced.set_paced(true);
    paced.send_black_bgra(1920, 1080);
    let expected = floor_boundary_100ns(fake_now, GENLOCK_GRID_FPS);
    assert_eq!(
        backend2.video_timecodes(),
        vec![expected],
        "paced standby frame must carry its on-grid boundary"
    );
    assert_ne!(
        expected,
        sp_ndi::NDI_SEND_TIMECODE_SYNTHESIZE,
        "the on-grid stamp must not collide with SYNTHESIZE"
    );
}
