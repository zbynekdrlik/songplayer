//! Boundary-paced emission — lane-4 tests (#147): decode AHEAD of the boundary.
//!
//! Box test 2 (2026-09-13 16:44, `genlock_pacing=true`, lyrics worker paused)
//! showed the playing output holding 31/s and re-anchoring correctly, BUT
//! `late_frames` +25/s: `pipeline_paced` slept to the boundary and only THEN
//! decoded the due frame (`iter_p99` 10-27 ms), so every frame LEFT the box
//! 10-27 ms after the boundary it was stamped with — past the receiver's
//! `wall_now − 3 ms` release point (`ts_head_skew_ms` ≈ 74). Audio was pushed at
//! the same late instant (12 underruns / 4 min).
//!
//! Lane 4 splits the pacer's per-boundary work: [`Pacer::prepare`] decodes
//! forward to the frame due at the NEXT boundary (dropping older, parking the
//! first beyond, pushing every consumed frame's audio into the wall-clock
//! buffer) and is called right AFTER each emit; [`Pacer::service`] then only
//! emits the pre-decoded frame at the boundary. With the decode off the critical
//! path the emit lands within ~1 ms of its stamp and the boundary audio chunk is
//! always already buffered.
//!
//! RED half: these reference [`Pacer::prepare`], [`Pacer::next_boundary_100ns`],
//! [`Pacer::prep_p99_us`] and the new `PacingStats.prep_p99_us` field — none of
//! which exist until GREEN (a compile-failure RED, the accepted shape). `super::*`
//! resolves to the `pacer` module under test.

use super::*;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::cell::RefCell;
use std::collections::VecDeque;

/// The first grid boundary after 0 — `wall_start` after `anchor()` at clock 0.
const B1: i64 = 333_333;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

fn mk_frame(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![],
    }
}

/// A frame due exactly at `target_100ns` given `wall_start = B1`.
fn frame_due_at(target_100ns: i64) -> PacedFrame {
    mk_frame((target_100ns - B1) * 100)
}

/// A frame due at `present_100ns` carrying `n` stereo audio samples.
fn frame_due_with_audio(present_100ns: i64, n: usize) -> PacedFrame {
    let pts_ns = (present_100ns - B1) * 100;
    let mut data = Vec::with_capacity(n * 2);
    for _ in 0..n {
        data.push(0.25f32);
        data.push(-0.25f32);
    }
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![AudioFrame {
            data,
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// Records per emit the video timecode (the on-grid boundary stamp), the audio
/// timecode (raw emit-instant wall clock), and the per-channel audio sample count
/// of the boundary chunk.
#[derive(Default)]
struct RecordingSink {
    video_tcs: Vec<i64>,
    audio_tcs: Vec<i64>,
    audio_samples: Vec<usize>,
}

impl PacedSink for RecordingSink {
    fn emit(
        &mut self,
        _video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        self.video_tcs.push(video_tc_100ns);
        self.audio_tcs.push(audio_tc_100ns);
        let samples: usize = audio
            .iter()
            .map(|a| {
                if a.channels > 0 {
                    a.data.len() / a.channels as usize
                } else {
                    0
                }
            })
            .sum();
        self.audio_samples.push(samples);
    }
}

/// A pacer over a settable clock, anchored at clock 0 (wall_start = [`B1`]).
fn anchored_pacer() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

// ---------------------------------------------------------------------------
// (1) Decode-ahead keeps the emit ON its boundary (≤ 1 ms) with a 20 ms decode
// cost — the box-test-2 fix. Before lane 4 the emit landed ≈ decode-cost late.
// ---------------------------------------------------------------------------

#[test]
fn decode_ahead_keeps_emit_on_boundary_with_20ms_decode_cost() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    // 30-fps content: frame j due at b(j+1).
    let frames = RefCell::new(VecDeque::new());
    for j in 0..320i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    // Each decoder pull costs 20 ms (200_000 × 100 ns), advanced inside the pull.
    let pull = |frames: &RefCell<VecDeque<PacedFrame>>| {
        clk2.advance(200_000);
        frames.borrow_mut().pop_front()
    };
    let mut sink = RecordingSink::default();

    // Cold-start decode-ahead: in production the first frame(s) decode before the
    // wall grid begins advancing, so the startup burst is absorbed by the sleep.
    let first = pacer.next_boundary_100ns();
    pacer.prepare(first, || pull(&frames));
    // The pre-roll is done; the wall grid starts at the first boundary.
    clk.set(B1);

    let mut emits = 0;
    for _ in 0..300 {
        let target = pacer.next_boundary_100ns();
        // "Sleep" to the boundary (respecting an overrun where decode ran past it).
        clk.set(target.max(clk.get()));
        let out = pacer.service(|| None, &mut sink);
        assert_eq!(
            out,
            ServiceOutcome::Emitted,
            "one pre-decoded emit per boundary"
        );
        emits += 1;
        // Decode the NEXT due frame immediately after the emit (off the sleep).
        let next = pacer.next_boundary_100ns();
        pacer.prepare(next, || pull(&frames));
    }

    assert_eq!(emits, 300);
    let stats = pacer.stats();
    assert_eq!(
        stats.late_frames, 0,
        "with the decode moved ahead of the boundary NO emit is late"
    );
    // Every emit lands within ~1 ms of its boundary (the 100-ns grid rounding);
    // before lane 4 this gap was ≈ the 20 ms decode cost on every boundary.
    for (v, a) in sink.video_tcs.iter().zip(&sink.audio_tcs) {
        let late_100ns = a - v;
        assert!(
            (0..=10_000).contains(&late_100ns),
            "emit within 1 ms of its boundary: video_tc={v} audio_tc={a} late={late_100ns}"
        );
    }
    // The pre-decode duration is surfaced (≈ 20 ms per boundary).
    assert!(
        (15_000..=45_000).contains(&stats.prep_p99_us),
        "prep_p99 reflects the ~20 ms decode-ahead cost, got {}",
        stats.prep_p99_us
    );
    // iter_p99 keeps measuring decode+submit per boundary (decode now in prepare).
    assert!(
        pacer.iter_p99_us() >= 15_000,
        "iter_p99 still reflects the decode cost, got {}",
        pacer.iter_p99_us()
    );
}

// ---------------------------------------------------------------------------
// (2) The boundary audio chunk is pushed DURING prepare, so it is always
// buffered at the take: no underruns over 300 boundaries of 23.976 content.
// ---------------------------------------------------------------------------

#[test]
fn pre_decoded_audio_present_at_every_boundary_no_underruns() {
    let (mut pacer, clk) = anchored_pacer();
    // 23.976-fps content on the 30-fps grid: a frame every ~417_083 (100 ns).
    // Steady frames carry the 23.976 payload (2002 samples); the FIRST frame
    // carries the decoder's startup audio lead (in production the FLAC decoder
    // front-loads several packets before the wall grid starts).
    let cap = 417_083i64;
    let frames = RefCell::new(VecDeque::new());
    frames
        .borrow_mut()
        .push_back(frame_due_with_audio(B1, 5000));
    for j in 1..320i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_with_audio(B1 + j * cap, 2002));
    }
    let pull = |frames: &RefCell<VecDeque<PacedFrame>>| frames.borrow_mut().pop_front();
    let mut sink = RecordingSink::default();

    let first = pacer.next_boundary_100ns();
    pacer.prepare(first, || pull(&frames));
    clk.set(B1);

    let mut boundaries = 0;
    for _ in 0..300 {
        let target = pacer.next_boundary_100ns();
        clk.set(target.max(clk.get()));
        let out = pacer.service(|| None, &mut sink);
        assert!(
            matches!(out, ServiceOutcome::Emitted | ServiceOutcome::Repeated),
            "every boundary is serviced (emit or sub-grid repeat), got {out:?}"
        );
        boundaries += 1;
        let next = pacer.next_boundary_100ns();
        pacer.prepare(next, || pull(&frames));
    }

    assert_eq!(boundaries, 300);
    let a = pacer.audio_stats();
    assert_eq!(
        a.underruns, 0,
        "pre-decoded audio is buffered before every take — no underruns"
    );
    assert_eq!(a.overflows, 0, "the buffer never overflows its 2 s cap");
    // Every serviced boundary submitted exactly one full boundary audio chunk.
    for (i, &s) in sink.audio_samples.iter().enumerate() {
        assert_eq!(s, 1600, "boundary {i}: exactly samples_per_boundary");
    }
}

// ---------------------------------------------------------------------------
// (3) A 60-fps source decode-ahead still yields exactly one emit per slot with
// the decimated frames counted in `dropped`.
// ---------------------------------------------------------------------------

#[test]
fn sixty_fps_source_one_emit_per_slot_via_prepare_with_dropped_counted() {
    let (mut pacer, clk) = anchored_pacer();
    // 60-fps frames every 166_666 (100 ns): two consumed per 30-fps boundary
    // (one emitted, one dropped).
    let cap = 166_666i64;
    let frames = RefCell::new(VecDeque::new());
    for j in 0..640i64 {
        frames.borrow_mut().push_back(frame_due_at(B1 + j * cap));
    }
    let pull = |frames: &RefCell<VecDeque<PacedFrame>>| frames.borrow_mut().pop_front();
    let mut sink = RecordingSink::default();

    let first = pacer.next_boundary_100ns();
    pacer.prepare(first, || pull(&frames));
    clk.set(B1);

    let mut emits = 0;
    for _ in 0..300 {
        let target = pacer.next_boundary_100ns();
        clk.set(target.max(clk.get()));
        if pacer.service(|| None, &mut sink) == ServiceOutcome::Emitted {
            emits += 1;
        }
        let next = pacer.next_boundary_100ns();
        pacer.prepare(next, || pull(&frames));
    }

    assert_eq!(
        emits, 300,
        "exactly one emit per boundary from a 60-fps source"
    );
    assert!(
        pacer.stats().dropped >= 290,
        "≈ one decimated frame per boundary counted in dropped, got {}",
        pacer.stats().dropped
    );
    // Stamps strictly increase by one grid slot.
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(
            step == 333_333 || step == 333_334,
            "one grid slot: step={step}"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) A decode cost OVER the interval (40 ms > 33.3 ms) still catches up and
// re-anchors as before, and the stamps stay ≤ now (never future-dated). The
// prepare-fed lag machinery is the same as lane 3.
// ---------------------------------------------------------------------------

#[test]
fn decode_cost_over_interval_catches_up_and_reanchors_stamps_le_now() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    let frames = RefCell::new(VecDeque::new());
    for j in 0..400i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    // 40 ms decode cost per pull (> the 33.3 ms interval): lag grows until the
    // playback re-anchor bounds it.
    let pull = |frames: &RefCell<VecDeque<PacedFrame>>| {
        clk2.advance(400_000);
        frames.borrow_mut().pop_front()
    };
    let mut sink = RecordingSink::default();

    let first = pacer.next_boundary_100ns();
    pacer.prepare(first, || pull(&frames));

    let mut guard = 0;
    while pacer.stats().seq < 200 && guard < 5000 {
        guard += 1;
        let target = pacer.next_boundary_100ns();
        clk.set(target.max(clk.get()));
        let out = pacer.service(|| None, &mut sink);
        // On a re-anchor the pacer emits nothing this call; the loop just
        // continues (the next prepare + service resume from the buffered frame).
        let _ = out;
        let next = pacer.next_boundary_100ns();
        pacer.prepare(next, || pull(&frames));
    }

    let stats = pacer.stats();
    assert!(
        stats.resyncs >= 1,
        "a persistent over-interval decode must re-anchor at least once, resyncs={}",
        stats.resyncs
    );
    // Every stamp is never future-dated (≤ the emit-instant wall clock).
    for (v, a) in sink.video_tcs.iter().zip(&sink.audio_tcs) {
        assert!(*v <= *a, "future-dated stamp: video_tc={v} audio_tc={a}");
    }
    assert!(
        pacer.iter_p99_us() >= 33_333,
        "iter_p99 shows the decoder can't keep up"
    );
}

// ---------------------------------------------------------------------------
// (5) prep_p99_us serialises on the /api/v1/ndi/health snapshot.
// ---------------------------------------------------------------------------

#[test]
fn prep_p99_us_serialises_on_the_snapshot() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    let frames = RefCell::new(VecDeque::new());
    for j in 0..10i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let pull = |frames: &RefCell<VecDeque<PacedFrame>>| {
        clk2.advance(200_000); // 20 ms decode
        frames.borrow_mut().pop_front()
    };

    let first = pacer.next_boundary_100ns();
    pacer.prepare(first, || pull(&frames));
    clk.set(B1);
    for _ in 0..5 {
        let target = pacer.next_boundary_100ns();
        clk.set(target.max(clk.get()));
        pacer.service(|| None, &mut RecordingSink::default());
        let next = pacer.next_boundary_100ns();
        pacer.prepare(next, || pull(&frames));
    }

    let stats = pacer.stats();
    let json = serde_json::to_value(&stats).unwrap();
    assert!(
        json.get("prep_p99_us").is_some(),
        "PacingStats must serialise prep_p99_us"
    );
    let back: crate::playback::ndi_health::PacingStats = serde_json::from_value(json).unwrap();
    assert_eq!(back, stats);
    assert!(
        pacer.prep_p99_us() >= 15_000,
        "prep_p99 reflects the decode-ahead cost"
    );
}

// ---------------------------------------------------------------------------
// (6) Standby (paused/idle) is unchanged by the split: service_standby never
// consumes a pre-decoded frame, and a paused pacer keeps repeating the frozen
// last real frame on the grid.
// ---------------------------------------------------------------------------

#[test]
fn standby_unaffected_by_a_pre_decoded_frame() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    // Emit a real frame so there is a frozen last frame.
    let target = pacer.next_boundary_100ns();
    let mut f1 = Some(frame_due_at(b(1)));
    pacer.prepare(target, || f1.take());
    clk.set(b(1));
    assert_eq!(pacer.service(|| None, &mut sink), ServiceOutcome::Emitted);

    // Pre-decode a NEXT frame that must NOT leak into standby.
    let next = pacer.next_boundary_100ns();
    let mut f2 = Some(frame_due_at(b(2)));
    pacer.prepare(next, || f2.take());

    // Now pause: standby repeats the frozen last frame every boundary.
    for k in 2..=20i64 {
        clk.set(b(k));
        assert_eq!(
            pacer.service_standby(Standby::FrozenLast, &mut sink),
            ServiceOutcome::Repeated,
            "paused standby repeats the frozen frame, never the pre-decoded one"
        );
    }
    // One real emit + 19 standby repeats, one stamp per boundary.
    assert_eq!(sink.video_tcs.len(), 20);
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(
            step == 333_333 || step == 333_334,
            "one grid slot: step={step}"
        );
    }
    assert!(
        sink.audio_samples[1..].iter().all(|&n| n == 0),
        "standby submits no audio"
    );
}
