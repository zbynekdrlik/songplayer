//! Boundary-paced emission scheduler tests (#147 rework — exact 100-ns grid).
//!
//! Drive the pure [`Pacer`] with a settable wall clock (never a real `sleep`), a
//! synthetic PTS stream and a recording sink, so the full
//! emit/repeat/drop/catch-up/resync/re-latch behaviour runs on the Linux CI
//! job. `super::*` resolves to the `pacer` module under test.
//!
//! These REPLACE the pre-rework tests that encoded the defective behaviour: the
//! uniform-ns grid, the `floor(now)`-at-emission stamp, the future-dating
//! "monotonicity guard", and the `service(now, …)` signature that took the
//! scheduling instant as a parameter. The rework paces on the exact 100-ns grid
//! (stamp = the serviced boundary itself), reads the wall clock internally
//! (scheduling read + a fresh emit read after decode), and deletes the guard.

use super::*;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;

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

fn mk_frame_with_audio(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: vec![0u8; 12],
        audio: vec![AudioFrame {
            data: vec![0.1, 0.2, 0.3, 0.4],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// A frame whose presentation time lands exactly on `target_100ns` given a
/// `wall_start` of [`B1`] (i.e. `anchor()` was called at clock 0).
fn frame_due_at(target_100ns: i64) -> PacedFrame {
    mk_frame((target_100ns - B1) * 100)
}

fn frame_due_at_audio(target_100ns: i64) -> PacedFrame {
    mk_frame_with_audio((target_100ns - B1) * 100)
}

/// A recording sink: captures per emit the video/audio timecodes and the
/// per-channel sample count of the boundary audio chunk (#148 — audio is the
/// wall-clock buffer's boundary chunk, no longer one chunk per consumed frame).
#[derive(Default)]
struct RecordingSink {
    video_tcs: Vec<i64>,
    audio_tcs: Vec<i64>,
    /// Per-channel sample count of the audio batch handed to each emit.
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
// Exact-grid stamping: the serviced boundary IS the stamp (no floor(now)).
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_emit_per_boundary_over_100_boundaries() {
    let (mut pacer, clk) = anchored_pacer();
    // Frame j is due exactly at boundary b(j+1).
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..120i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let mut sink = RecordingSink::default();

    let mut emits = 0;
    for k in 1..=100i64 {
        clk.set(b(k));
        let outcome = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
        if outcome == ServiceOutcome::Emitted {
            emits += 1;
        }
    }

    assert_eq!(emits, 100, "exactly one emit per boundary");
    assert_eq!(pacer.stats().seq, 100);
    assert_eq!(
        pacer.stats().dropped,
        0,
        "one-frame-per-boundary drops nothing"
    );
    // Stamps are the serviced boundaries: b(1)..b(100), each one grid slot on.
    assert_eq!(sink.video_tcs.len(), 100);
    assert_eq!(sink.video_tcs[0], b(1));
    assert_eq!(sink.video_tcs[99], b(100));
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(step == 333_333 || step == 333_334, "step={step}");
    }
}

#[test]
fn stamp_is_the_serviced_boundary_never_floor_of_now() {
    // Service well PAST the boundary (late by ~7 ms): the stamp must be the
    // boundary itself, never floor(now) — and never future-dated.
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1) + 70_000); // 7 ms past b(1)
    pacer.service(|| first.take(), &mut sink);
    assert_eq!(sink.video_tcs, vec![b(1)], "stamp is the boundary");
    assert!(sink.video_tcs[0] <= sink.audio_tcs[0], "never future-dated");
    assert_eq!(
        sink.audio_tcs,
        vec![b(1) + 70_000],
        "audio = raw wall clock"
    );
}

// ---------------------------------------------------------------------------
// Audio decoupled from the video decision (#147 change 3).
// ---------------------------------------------------------------------------

// CORRECTED for #148: audio is no longer one chunk per consumed frame — the
// pacer pushes every consumed frame's audio into its wall-clock AudioGridBuffer
// and submits exactly `samples_per_boundary` (1600 @ 48 kHz/30) per boundary,
// decoupled from the video frame rate. These two tests hard-coded the pre-#148
// per-frame pass-through (chunk count == consumed frames; a repeat submits no
// audio) — both premises are gone. See `pacer_tests_audio.rs` for the full
// audio-clock wiring coverage.

#[test]
fn every_emit_submits_exactly_one_boundary_audio_chunk_over_a_60fps_source() {
    let (mut pacer, clk) = anchored_pacer();
    // 60 fps frames every 166_666 (100 ns): two consumed per 30-fps boundary
    // (one emitted, one dropped/decimated). Audio is the buffer's boundary
    // chunk, so each emit submits ONE 1600-sample chunk regardless of how many
    // frames were consumed.
    let cap = 166_666i64;
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..60i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_at_audio(B1 + j * cap));
    }
    let mut sink = RecordingSink::default();

    let mut emits = 0;
    for k in 1..=30i64 {
        clk.set(b(k));
        if pacer.service(|| frames.borrow_mut().pop_front(), &mut sink) == ServiceOutcome::Emitted {
            emits += 1;
        }
    }

    let dropped = pacer.stats().dropped;
    assert!((29..=31).contains(&emits), "60->30 emits: {emits}");
    assert!(dropped >= 25, "≈30 frames decimated, got {dropped}");
    // Video decimation is unchanged, but audio is one boundary chunk per emit.
    for (i, &s) in sink.audio_samples.iter().enumerate() {
        assert_eq!(s, 1600, "boundary {i}: exactly samples_per_boundary");
    }
}

#[test]
fn sub_grid_repeats_still_submit_a_boundary_audio_chunk() {
    let (mut pacer, clk) = anchored_pacer();
    // 23.976 fps frames (interval ~417_083 100 ns > one 30-fps slot) so some
    // boundaries have no due frame and REPEAT the video. Audio is DECOUPLED from
    // the video decision (#148), so a repeat still submits the boundary's
    // 1600-sample chunk from the buffer.
    let cap = 417_083i64;
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..40i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_at_audio(B1 + j * cap));
    }
    let mut sink = RecordingSink::default();

    let mut repeats = 0;
    for k in 1..=30i64 {
        clk.set(b(k));
        let outcome = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
        if outcome == ServiceOutcome::Repeated {
            repeats += 1;
            assert_eq!(
                *sink.audio_samples.last().unwrap(),
                1600,
                "a video repeat still submits a 1600-sample audio chunk (#148)"
            );
        }
    }
    assert!(repeats > 0, "sub-grid content must repeat");
    // Stamps still strictly increase by one grid slot (repeat = serviced boundary).
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(step == 333_333 || step == 333_334, "step={step}");
    }
}

#[test]
fn audio_is_submitted_before_video_at_every_boundary() {
    use sp_ndi::MockNdiBackend;
    use sp_ndi::NdiSender;
    use std::sync::Arc;

    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "Paced", false, false).unwrap();
    let mut submitter = crate::playback::submitter::FrameSubmitter::new(sender, 30, 1);

    let (mut pacer, clk) = anchored_pacer();
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..30i64 {
        frames.borrow_mut().push_back(frame_due_at_audio(b(j + 1)));
    }

    for k in 1..=10i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut submitter);
    }

    let calls = backend.calls();
    let mut saw_audio_since_video = false;
    let mut video_count = 0;
    for c in &calls {
        if c.starts_with("send_audio") {
            saw_audio_since_video = true;
        } else if c.starts_with("send_video_async") {
            assert!(
                saw_audio_since_video,
                "audio must precede each paced video frame: {calls:#?}"
            );
            saw_audio_since_video = false;
            video_count += 1;
        }
    }
    assert_eq!(video_count, 10, "one paced video frame per boundary");
    let tcs = backend.video_timecodes();
    for w in tcs.windows(2) {
        assert!(w[1] > w[0], "paced video timecodes must strictly increase");
    }
}

// ---------------------------------------------------------------------------
// Underrun / catch-up / resync / re-latch.
// ---------------------------------------------------------------------------

#[test]
fn underrun_repeats_last_frame_stamped_at_new_boundary() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    assert_eq!(
        pacer.service(|| first.take(), &mut sink),
        ServiceOutcome::Emitted
    );

    clk.set(b(2));
    assert_eq!(
        pacer.service(|| None, &mut sink),
        ServiceOutcome::Repeated,
        "no new frame -> repeat"
    );
    assert_eq!(pacer.stats().repeats, 1);
    assert_eq!(sink.video_tcs, vec![b(1), b(2)]);
    assert!(
        sink.video_tcs[1] > sink.video_tcs[0],
        "repeat stamp advances to the new boundary"
    );
}

#[test]
fn late_by_three_intervals_catches_up_then_on_time() {
    // NOTE: this replaces the pre-rework test that (in the defective design)
    // could assert future-dated stamps; here the clock is fixed 3 slots late
    // and each catch-up emit is stamped with its own (past) boundary.
    let (mut pacer, clk) = anchored_pacer();
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..10i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let mut sink = RecordingSink::default();

    // The thread stalled: `now` is 3 slots past the first boundary. Service
    // repeatedly at the SAME now — the pacer catches up one slot per emit.
    clk.set(b(4));
    for _ in 0..4 {
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }

    assert_eq!(
        pacer.stats().late_frames,
        3,
        "three catch-up emits are late"
    );
    assert_eq!(pacer.stats().resyncs, 0, "a bounded catch-up never resyncs");
    assert_eq!(pacer.stats().seq, 4);
    // Stamps are the caught-up boundaries b(1)..b(4), each never future-dated.
    assert_eq!(sink.video_tcs, vec![b(1), b(2), b(3), b(4)]);
    for tc in &sink.video_tcs {
        assert!(*tc <= b(4), "never future-dated: tc={tc}");
    }
    // The 5th call has caught up -> Wait.
    clk.set(b(4));
    assert!(matches!(
        pacer.service(|| None, &mut sink),
        ServiceOutcome::Wait { .. }
    ));
}

#[test]
fn late_by_twelve_with_nothing_pending_resyncs() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    // Emit one frame so there is a last frame to repeat.
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    pacer.service(|| first.take(), &mut sink);
    assert_eq!(pacer.stats().resyncs, 0);

    // Long stall, NO frames available (underrun): now jumps ~13 slots.
    clk.set(b(14));
    let o = pacer.service(|| None, &mut sink);
    assert_eq!(o, ServiceOutcome::Repeated);
    assert_eq!(
        pacer.stats().resyncs,
        1,
        "lag>8 with nothing buffered resyncs"
    );

    // Fix-lane-2 (§5.5): the resync repeat is stamped at the RESYNC service
    // boundary (the grid boundary at/before now), NEVER the stale pre-stall
    // boundary b(2) ~430 ms in the past. The old code left this un-asserted and
    // shipped the stale stamp, which the receiver answers with
    // late_holds/dropped_due (A8.1 requires 0).
    let interval = sp_core::genlock::interval_100ns(30);
    let now_floor = sp_core::genlock::floor_boundary_100ns(b(14), 30);
    let stamp = *sink.video_tcs.last().unwrap();
    assert!(
        stamp >= now_floor - interval,
        "resync repeat must be at the resync boundary, not stale: stamp={stamp} now_floor={now_floor}"
    );
    assert!(stamp <= b(14), "never future-dated: stamp={stamp}");
    assert_eq!(
        sp_core::genlock::floor_boundary_100ns(stamp, 30),
        stamp,
        "resync stamp on-grid"
    );
    assert_ne!(stamp, b(2), "must not be the stale pre-stall boundary");
}

#[test]
fn backward_clock_step_relatches() {
    let (wall, clk) = WallClock::settable(100 * b(1));
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(b(100));
    pacer.anchor(); // wall_start = strict_next(b(100))
    let mut sink = RecordingSink::default();

    let mut first = Some(mk_frame(0));
    clk.set(b(101));
    pacer.service(|| first.take(), &mut sink);
    assert_eq!(pacer.stats().relatches, 0);

    // Clock steps backward far below the latched boundary.
    clk.set(b(50));
    let o = pacer.service(|| None, &mut sink);
    assert!(matches!(o, ServiceOutcome::Wait { .. }));
    assert_eq!(
        pacer.stats().relatches,
        1,
        "a backward step re-latches once"
    );
}

// ---------------------------------------------------------------------------
// Never future-dated across a mixed run (#147 change 2).
// ---------------------------------------------------------------------------

#[test]
fn never_future_dated_over_a_mixed_run_with_catchup_and_resync() {
    let (mut pacer, clk) = anchored_pacer();
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    // Frames due at b(1)..b(25); phases A/B/C consume all 25.
    for m in 1..=25i64 {
        frames.borrow_mut().push_back(frame_due_at(b(m)));
    }
    let mut sink = RecordingSink::default();

    // Phase A: on-time b(1)..b(15).
    for k in 1..=15i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }
    // Phase B: late-by-3 burst — clock 3 slots ahead, 3 catch-ups (b16..b18).
    clk.set(b(19));
    for _ in 0..3 {
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }
    // Phase C: on-time b(19)..b(25).
    for k in 19..=25i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }
    // Phase D: starvation resync — clock 12 slots past b(26), nothing buffered.
    clk.set(b(38));
    assert_eq!(pacer.service(|| None, &mut sink), ServiceOutcome::Repeated);
    // Phase E: resume with a frame due at the resynced boundary b(39).
    frames.borrow_mut().push_back(frame_due_at(b(39)));
    clk.set(b(39));
    pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);

    let stats = pacer.stats();
    assert!(
        stats.late_frames >= 3,
        "catch-up exercised: {}",
        stats.late_frames
    );
    assert_eq!(stats.resyncs, 1, "exactly one resync");

    // Invariant 1: every stamp <= the emit-instant wall clock (audio_tc).
    for (v, a) in sink.video_tcs.iter().zip(&sink.audio_tcs) {
        assert!(*v <= *a, "future-dated stamp: video_tc={v} audio_tc={a}");
    }
    // Invariant 2: stamps strictly increase; each step is one grid slot EXCEPT
    // across a resync, and there is exactly one such multi-slot step.
    let mut multi_slot = 0;
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(step > 0, "stamps strictly increase: step={step}");
        if step != 333_333 && step != 333_334 {
            multi_slot += 1;
        }
    }
    assert_eq!(
        multi_slot, stats.resyncs as usize,
        "only a resync makes a multi-slot stamp gap"
    );
}

// ---------------------------------------------------------------------------
// Lateness measured at the submit instant (#147 change 5).
// ---------------------------------------------------------------------------

#[test]
fn lateness_measured_at_the_submit_instant_includes_decode_time() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let clk2 = clk.clone();

    clk.set(B1); // sched_now = b(1), exactly on the boundary
    let mut served = false;
    pacer.service(
        || {
            if served {
                return None;
            }
            served = true;
            clk2.advance(50_000); // +5 ms "decode" before the emit read
            Some(frame_due_at(B1))
        },
        &mut sink,
    );

    // Stamp is the boundary; audio (emit read) is 5 ms past it.
    assert_eq!(sink.video_tcs, vec![B1]);
    assert_eq!(sink.audio_tcs, vec![B1 + 50_000]);
    let stats = pacer.stats();
    assert!(
        stats.max_late_us >= 5_000,
        "lateness must include decode time: max_late_us={}",
        stats.max_late_us
    );
}

#[test]
fn jitter_p99_is_computed_from_the_ring() {
    let (mut pacer, clk) = anchored_pacer();
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..30i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let mut sink = RecordingSink::default();

    // Service each boundary a fixed 7 µs (70 × 100 ns) late.
    for k in 1..=20i64 {
        clk.set(b(k) + 70);
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }

    let stats = pacer.stats();
    assert_eq!(stats.jitter_p99_us, 7, "p99 of a constant 7 µs jitter is 7");
    assert_eq!(stats.late_frames, 0, "7 µs is well under one interval");
    assert!(stats.max_late_us >= 7);
}

// ---------------------------------------------------------------------------
// Telemetry + genlock-off guard.
// ---------------------------------------------------------------------------

#[test]
fn counters_serialise_on_the_snapshot() {
    let (mut pacer, clk) = anchored_pacer();
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    pacer.service(|| first.take(), &mut RecordingSink::default());
    clk.set(b(2));
    pacer.service(|| None, &mut RecordingSink::default()); // a repeat

    let stats = pacer.stats();
    assert!(stats.enabled);
    let json = serde_json::to_value(&stats).unwrap();
    for field in [
        "enabled",
        "seq",
        "late_frames",
        "max_late_us",
        "jitter_p99_us",
        "repeats",
        "resyncs",
        "relatches",
        "dropped",
        "lag_slots",
        "iter_p99_us",
        "prep_p99_us",
    ] {
        assert!(json.get(field).is_some(), "missing pacing field {field}");
    }
    assert_eq!(json["repeats"].as_u64(), Some(1));
    let back: crate::playback::ndi_health::PacingStats = serde_json::from_value(json).unwrap();
    assert_eq!(back, stats);
}

#[test]
fn disabled_pacer_reports_default_stats() {
    let pacer = Pacer::with_wallclock(30, false, WallClock::fixed(0));
    let stats = pacer.stats();
    assert!(!stats.enabled);
    assert_eq!(stats, crate::playback::ndi_health::PacingStats::default());
}

#[test]
fn genlock_off_pacer_waits_one_second_never_spins() {
    // grid_fps 0 -> interval 0 -> the pacer must return a 1 s wait, never a
    // zero/near-zero wait that would spin the loop (#147 change 8).
    let (wall, clk) = WallClock::settable(500);
    let mut pacer = Pacer::with_wallclock(0, true, wall);
    clk.set(500);
    let out = pacer.service(|| None, &mut RecordingSink::default());
    assert_eq!(
        out,
        ServiceOutcome::Wait {
            until_100ns: 500 + 10_000_000
        }
    );
}

// ---------------------------------------------------------------------------
// Pure sleep-plan decision (#147 change 4).
// ---------------------------------------------------------------------------

#[test]
fn plan_sleep_clamps_to_one_second_and_flags_backward_relatch() {
    let i = 333_333;
    // Normal wait within one interval: sleep the delta, no relatch.
    assert_eq!(
        plan_sleep_100ns(1000, 1000 + 200_000, i),
        SleepDecision {
            sleep_100ns: 200_000,
            relatch: false
        }
    );
    // At/past the boundary: zero sleep, no relatch.
    assert_eq!(plan_sleep_100ns(5000, 4000, i).sleep_100ns, 0);
    assert!(!plan_sleep_100ns(5000, 4000, i).relatch);
    // Boundary more than one interval ahead (a backward jump / anomaly):
    // relatch, and the coarse sleep is still clamped to 1 s.
    let d = plan_sleep_100ns(0, 50_000_000, i);
    assert!(d.relatch);
    assert_eq!(d.sleep_100ns, 10_000_000);
    // interval == 0 (genlock off): never relatch; clamped 1 s sleep.
    let d0 = plan_sleep_100ns(0, 50_000_000, 0);
    assert!(!d0.relatch);
    assert_eq!(d0.sleep_100ns, 10_000_000);
}

// ---------------------------------------------------------------------------
// Structural guard: the flag-OFF (legacy SDK-clocked) decode path is unchanged.
// Replaces the pre-rework two-string grep with an exact-substring assertion on
// the WHOLE legacy `decode_and_send` call site (#147 change 8).
// ---------------------------------------------------------------------------

#[test]
fn genlock_pacing_off_keeps_the_legacy_sdk_clocked_call_site() {
    // CRLF-normalise so the assertion is line-ending agnostic.
    let src = include_str!("pipeline.rs").replace("\r\n", "\n");

    // 1. Both sender clockings are present (paced = clock_video=false, legacy =
    //    clock_video=true), so the flag genuinely selects the path.
    assert!(
        src.contains("new_with_clocking(backend, ndi_name, true, false)"),
        "flag-OFF must still create the SDK-clocked sender (clock_video=true)"
    );
    assert!(
        src.contains("new_with_clocking(backend, ndi_name, false, false)"),
        "flag-ON must create the app-clocked sender (clock_video=false)"
    );

    // 2. The WHOLE legacy `decode_and_send` call site is byte-unchanged — a
    //    stronger guard than a two-string grep: any edit to how the legacy path
    //    is invoked (args, order) breaks this.
    // #15 part 2: the legacy call site now also passes the preview tap as its
    // final arg (an opportunistic sampler that never touches the NDI submit
    // path); the guard is updated to the new byte-exact call site.
    // #192: the SDK-clocked path now also passes the wall-clock audio emitter
    // (`audio_emitter.as_ref().map(|t| t.shared())`) as its final arg — audio is
    // pushed into that emitter's ring instead of riding the video submit — so
    // the byte-exact guard is updated to the new call site.
    let legacy_call_site = "\
                    } else {
                        decode_and_send(
                            &cmd_rx,
                            &mut submitter,
                            &current_video,
                            &current_audio,
                            &event_tx,
                            playlist_id,
                            &mut paused,
                            &mut last_heartbeat,
                            &mut consecutive_bad_polls,
                            current_start_ms,
                            &preview_tap,
                            audio_emitter.as_ref().map(|t| t.shared()),
                        )
                    };";
    assert!(
        src.contains(legacy_call_site),
        "the legacy decode_and_send call site must be byte-for-byte unchanged"
    );

    // 3. The legacy decode path still applies the per-file frame rate.
    let submitter_src = include_str!("submitter.rs").replace("\r\n", "\n");
    assert!(
        src.contains("set_frame_rate") || submitter_src.contains("set_frame_rate"),
        "the legacy path still calls set_frame_rate from the decoder"
    );
}

// ---------------------------------------------------------------------------
// #147 fix-lane-2 — RED
// ---------------------------------------------------------------------------

// Change 2: the paused/idle standby planner fills EVERY grid boundary with a
// frozen-last / black frame (on-grid, strictly increasing, never future-dated,
// no audio) via the same Pacer machinery, and a resuming Play re-anchors.

#[test]
fn standby_planner_fills_one_boundary_per_interval_no_audio() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    // Seed a real frame so the frozen-last standby has a picture to repeat.
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    assert_eq!(
        pacer.service(|| first.take(), &mut sink),
        ServiceOutcome::Emitted
    );

    // PAUSED: fill every boundary with the frozen last frame.
    let mut standby_emits = 0;
    for k in 2..=101i64 {
        clk.set(b(k));
        let outcome = pacer.service_standby(Standby::FrozenLast, &mut sink);
        assert_eq!(
            outcome,
            ServiceOutcome::Repeated,
            "paused standby repeats the frozen frame"
        );
        standby_emits += 1;
    }
    assert_eq!(standby_emits, 100);
    // One video stamp per boundary: 1 real + 100 standby.
    assert_eq!(sink.video_tcs.len(), 101);
    for w in sink.video_tcs.windows(2) {
        let step = w[1] - w[0];
        assert!(
            step == 333_333 || step == 333_334,
            "one grid slot: step={step}"
        );
    }
    for (v, a) in sink.video_tcs.iter().zip(&sink.audio_tcs) {
        assert!(*v <= *a, "never future-dated: video_tc={v} audio_tc={a}");
        assert_eq!(
            sp_core::genlock::floor_boundary_100ns(*v, 30),
            *v,
            "on-grid stamp"
        );
    }
    // Standby submits NO audio.
    assert!(
        sink.audio_samples.iter().all(|&n| n == 0),
        "standby emits no audio"
    );
    // Every frozen-frame repeat bumps `repeats`.
    assert_eq!(pacer.stats().repeats, 100);
}

#[test]
fn standby_black_fills_boundaries_without_a_last_frame() {
    // Idle / no song: no frame was ever decoded, so FrozenLast would starve;
    // Black fills the boundary with the supplied black frame instead.
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let black = mk_frame(0);

    for k in 1..=50i64 {
        clk.set(b(k));
        let out = pacer.service_standby(Standby::Black(&black), &mut sink);
        assert_eq!(out, ServiceOutcome::Emitted, "black idle frame is emitted");
    }
    assert_eq!(sink.video_tcs.len(), 50);
    for w in sink.video_tcs.windows(2) {
        assert!(w[1] > w[0], "black idle stamps strictly increase");
    }
    for (v, a) in sink.video_tcs.iter().zip(&sink.audio_tcs) {
        assert!(*v <= *a, "never future-dated");
    }
    assert!(
        sink.audio_samples.iter().all(|&n| n == 0),
        "idle emits no audio"
    );
    // Black idle frames are NOT frozen-frame repeats.
    assert_eq!(pacer.stats().repeats, 0);
}

#[test]
fn standby_then_resume_play_reanchors_cleanly() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    pacer.service(|| first.take(), &mut sink);
    for k in 2..=5i64 {
        clk.set(b(k));
        pacer.service_standby(Standby::FrozenLast, &mut sink);
    }

    // Resume Play: re-anchor at a far-later instant, then a pts-0 frame is due
    // at the new anchor boundary.
    clk.set(b(200));
    pacer.anchor();
    let want = sp_core::genlock::strict_next_boundary_100ns(b(200), 30);
    clk.set(want);
    let mut nf = Some(mk_frame(0));
    let out = pacer.service(|| nf.take(), &mut sink);
    assert_eq!(out, ServiceOutcome::Emitted, "resume re-anchors and emits");
    assert_eq!(
        *sink.video_tcs.last().unwrap(),
        want,
        "stamped at the new anchor boundary"
    );
}

// Change 4: anchor() clears last_frame so a cross-song repeat can never show
// the previous song's frame.
#[test]
fn anchor_clears_last_frame_so_no_cross_song_repeat() {
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();

    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    assert_eq!(
        pacer.service(|| first.take(), &mut sink),
        ServiceOutcome::Emitted
    );
    clk.set(b(2));
    assert_eq!(
        pacer.service(|| None, &mut sink),
        ServiceOutcome::Repeated,
        "last_frame present -> repeat"
    );

    // New song boundary: anchor() must drop the previous song's last_frame.
    clk.set(b(10));
    pacer.anchor();
    clk.set(sp_core::genlock::strict_next_boundary_100ns(b(10), 30));
    assert_eq!(
        pacer.service(|| None, &mut sink),
        ServiceOutcome::Starved,
        "anchor cleared last_frame -> no cross-song repeat"
    );
}

// Change 3: plan_sleep_100ns must tolerate the exact 333_334-wide slot without
// flagging a spurious backward-step relatch.
#[test]
fn plan_sleep_tolerates_the_exact_slot_width_on_wide_slots() {
    let fps = 30;
    let interval = sp_core::genlock::interval_100ns(fps); // 333_333
    // A boundary that precedes one of the ten 333_334-wide slots per second.
    let now = sp_core::genlock::floor_boundary_100ns(4_666_667, fps);
    let until = sp_core::genlock::strict_next_boundary_100ns(now, fps);
    assert_eq!(until - now, 333_334, "this is the wide slot");
    assert!(until - now > interval, "wider than the nominal interval");
    let d = plan_sleep_100ns(now, until, interval);
    assert!(
        !d.relatch,
        "the exact slot width must NOT trigger a spurious relatch"
    );
    assert_eq!(d.sleep_100ns, 333_334);
}
