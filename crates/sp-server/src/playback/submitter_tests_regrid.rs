//! #224 part 2 (design record 5899388193): the fleet labels go on at the ONE
//! submit edge. A paced pair's stamps are internal boundaries b; the wire
//! stamp is `floor_boundary(b + D(K_F))`, K_F read once per pair from the
//! relabel registry, so audio and video share it; it is never above the fleet
//! clock (the virtual clock's true UTC), whether the wall that serviced b has
//! followed the step yet or not. Pins derived with a scratch Python model.
//! Wired via `#[cfg(test)] #[path = "submitter_tests_regrid.rs"]` in
//! `submitter.rs`.

use std::sync::Arc;

use super::*;
use crate::playback::fleet_shift::{FleetShift, split, wire_stamp_100ns};
use crate::playback::wallclock::{VirtualClock, WallClock};
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;

/// One stereo boundary of audio.
fn block() -> Vec<AudioFrame> {
    vec![AudioFrame {
        data: vec![0.0; 3200],
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }]
}

/// A paced submitter whose wall follows `fleet` (its K_F is the registry's).
fn submitter(
    clk: &Arc<VirtualClock>,
    fleet: &Arc<FleetShift>,
) -> (FrameSubmitter<MockNdiBackend>, Arc<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "RG", false, false).unwrap();
    let wall = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    (
        FrameSubmitter::new_with_wallclock(sender, 30, 1, wall),
        backend,
    )
}

/// The last pair's (video, audio) wire stamps.
fn last_pair(backend: &MockNdiBackend) -> (i64, i64) {
    let v = *backend.video_timecodes().last().expect("a video send");
    let a = *backend.audio_timecodes().last().expect("an audio send");
    (v, a)
}

#[test]
fn wire_stamps_are_floored_and_never_above_the_fleet_clock_for_a_followed_and_a_lagging_wall() {
    // (step, K after it): +260.3 ms, −19.8 ms, the nightly −1.5 s, +90 ms.
    for (step, k) in [
        (2_603_000, 7),
        (-198_000, -1),
        (-15_000_000, -45),
        (900_000, 2),
    ] {
        let r = split(step, 0).remainder_100ns;
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        let wall = || WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
        // A pacer's wall that follows the step, and one that has not yet.
        let (mut followed, mut lagging) = (wall(), wall());
        let (mut sub, backend) = submitter(&clk, &fleet);
        for _ in 0..3 {
            clk.advance_ns(FRAME_NS);
            followed.tick();
            lagging.tick();
        }
        clk.step_utc(step);
        clk.advance_ns(FRAME_NS);
        followed.tick();
        assert_eq!(fleet.slots(), k, "{step}: registered");
        for i in 0..5 {
            // The followed wall services its next boundary on time.
            let b = strict_next_boundary_100ns(followed.now_100ns(), GENLOCK_GRID_FPS);
            clk.advance_ns(u64::try_from(b - followed.now_100ns()).unwrap() * 100);
            sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &block(), b, b);
            let (v, a) = last_pair(&backend);
            assert_eq!(v, a, "{step} #{i}: audio and video share K_F");
            assert_eq!(v, wire_stamp_100ns(b, k), "{step} #{i}: relabelled");
            assert_eq!(
                v,
                floor_boundary_100ns(clk.truth_100ns(), GENLOCK_GRID_FPS),
                "{step} #{i}: the fleet boundary at or before the fleet clock"
            );
            // The lagging wall's boundary is on the OLD timeline: its wire
            // stamp is up to r stale, never future-dated.
            let b2 = floor_boundary_100ns(lagging.now_100ns(), GENLOCK_GRID_FPS);
            sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &block(), b2, b2);
            let (v2, a2) = last_pair(&backend);
            assert_eq!(v2, a2, "{step} #{i}");
            assert_eq!(
                floor_boundary_100ns(v2, GENLOCK_GRID_FPS),
                v2,
                "{step} #{i}: on grid"
            );
            assert!(
                v2 + r <= clk.truth_100ns(),
                "{step} #{i}: a lagging wall's stamp {v2} is at least r = {r} behind the fleet clock {}",
                clk.truth_100ns()
            );
        }
    }
}

#[test]
fn a_forwarded_audio_stamp_keeps_its_offset_from_the_video_under_the_relabel() {
    // Review r1 🔴 A: `ProgramOutput::submit` forwards a source's OWN audio
    // stamp. The wire edge moves it by the video's relabel (K_F read once),
    // so its offset from the video survives — at K = 0 the stamp is
    // unchanged, never floored onto the grid (0 on every paced path, where
    // audio = video).
    for (step, k) in [(0i64, 0i64), (2_603_000, 7), (-198_000, -1)] {
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        if step != 0 {
            let _ = fleet.follow(0, step);
        }
        assert_eq!(fleet.slots(), k, "{step}");
        let (mut sub, backend) = submitter(&clk, &fleet);
        let b = floor_boundary_100ns(clk.truth_100ns(), GENLOCK_GRID_FPS);
        sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &block(), b, b + 20_077);
        let (v, a) = last_pair(&backend);
        assert_eq!(v, wire_stamp_100ns(b, k), "{step}: the video relabelled");
        assert_eq!(a - v, 20_077, "{step}: the source's own offset");
    }
}

#[test]
fn the_sdk_clocked_submit_puts_its_own_timeline_reading_back_on_the_fleet_labels() {
    for step in [2_603_000i64, -198_000] {
        // Its own wall follows the step at the submit's tick: the reading is
        // the fleet clock exactly.
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        let (mut sub, backend) = submitter(&clk, &fleet);
        clk.advance_ns(FRAME_NS);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &block());
        clk.step_utc(step);
        clk.advance_ns(FRAME_NS);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &block());
        let (v, a) = last_pair(&backend);
        assert_eq!(a, clk.truth_100ns(), "{step}: the raw fleet reading (§6)");
        assert_eq!(v, floor_boundary_100ns(a, GENLOCK_GRID_FPS), "{step}");
        // Another wall registered the step first, and the submitter's own
        // probe is preempted this tick: its reading is still the old
        // timeline, put on the NEW labels — r stale, never future-dated.
        let clk = VirtualClock::new(0);
        let fleet = Arc::new(FleetShift::default());
        let mut other = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
        let (mut sub, backend) = submitter(&clk, &fleet);
        clk.advance_ns(FRAME_NS);
        other.tick();
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &block());
        clk.step_utc(step);
        clk.advance_ns(FRAME_NS);
        other.tick();
        clk.delay_next_reads(&[400_000]);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &block());
        let (v, a) = last_pair(&backend);
        let r = split(step, 0).remainder_100ns;
        assert_eq!(a, clk.truth_100ns() - r, "{step}: r stale");
        assert_eq!(v, floor_boundary_100ns(a, GENLOCK_GRID_FPS), "{step}");
    }
}
