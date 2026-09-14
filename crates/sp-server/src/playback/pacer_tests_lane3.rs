//! Boundary-paced emission — fix-lane-3 tests (#147).
//!
//! Box test 1 (2026-09-13 01:43, `genlock_pacing=true`, all 8 outputs) showed
//! the PLAYING output settling at 27.4 emits/s with EVERY emit late,
//! `max_late_us` 40 s and growing, `jitter_p99` 15 s, `resyncs` 1. Diagnosis:
//! the exact-grid gate catches up at most ONE slot per `service()` call
//! (`resolve_emit_boundary` → `genlock_emit_gate_100ns`, camera-box's A5.6
//! "buffered never resyncs"), but each emitting `service()` call ALSO consumes
//! one decoder `pull` whose wall cost is `iter_cost`; when a file's per-frame
//! `iter_cost >= interval` the serviced boundary advances one interval per call
//! while the wall clock advances `iter_cost` per call, so the lag
//! `(now − next_boundary)` grows without bound and the stamps fall arbitrarily
//! far behind `now` (a growing NEGATIVE skew the receiver answers with
//! `dropped_due`/`late_holds`). Because a frame is always buffered, the
//! resync gate — which only fires on `!queue_had_frame` — never triggers;
//! camera-box's "buffered never resyncs" is a capture-side rule (a live grabber
//! can't outrun the wall clock), but a file decoder can fall behind, so for
//! playback the unbounded lag MUST be bounded by a wall re-anchor.
//!
//! These tests are the RED half of lane 3 (they reference the new
//! `lag_slots`/`iter_p99_us` telemetry, the `Reanchored` outcome and
//! `sp_core::genlock::lag_slots_100ns`, none of which exist until GREEN — a
//! compile-failure RED, the same accepted shape as the lane-1 RED).

use super::*;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// The first grid boundary after 0 (`wall_start` after `anchor()` at clock 0).
const B1: i64 = 333_333;

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

/// A frame whose presentation time lands exactly on `target_100ns` given a
/// `wall_start` of [`B1`] (i.e. `anchor()` was called at clock 0).
fn frame_due_at(target_100ns: i64) -> PacedFrame {
    mk_frame((target_100ns - B1) * 100)
}

/// A recording sink that also captures each emitted frame's `pts_ns` so a test
/// can prove content continuity (no frame skipped) across a re-anchor.
#[derive(Default)]
struct RecordingSink {
    video_tcs: Vec<i64>,
    audio_tcs: Vec<i64>,
    pts: Vec<i64>,
}

impl PacedSink for RecordingSink {
    fn emit(
        &mut self,
        video: &PacedFrame,
        _audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        self.video_tcs.push(video_tc_100ns);
        self.audio_tcs.push(audio_tc_100ns);
        self.pts.push(video.pts_ns);
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
// (a) Change 1 — a late serviced boundary emits IMMEDIATELY (no Wait/sleep),
// stamped with the serviced (past) boundary <= now; catch-up proceeds one slot
// per iteration.
// ---------------------------------------------------------------------------

#[test]
fn late_serviced_boundary_emits_immediately_never_waits() {
    let (mut pacer, clk) = anchored_pacer(); // wall_start=B1, next_boundary=b(1)
    let mut sink = RecordingSink::default();

    // Late by 5 slots with a frame buffered.
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(6));
    let out = pacer.service(|| first.take(), &mut sink);
    assert_eq!(
        out,
        ServiceOutcome::Emitted,
        "a due (past) boundary must emit, never Wait"
    );
    assert_eq!(sink.video_tcs, vec![b(1)], "stamp is the serviced boundary");
    assert!(
        sink.video_tcs[0] <= b(6),
        "never future-dated (stamp <= now)"
    );

    // Still 5 slots behind, no new frame -> repeat the last frame at the next
    // boundary (still <= now): again NO Wait, catch-up continues one slot/call.
    let out2 = pacer.service(|| None, &mut sink);
    assert_eq!(
        out2,
        ServiceOutcome::Repeated,
        "still behind -> emit immediately (repeat), never Wait"
    );
    assert!(*sink.video_tcs.last().unwrap() <= b(6), "still <= now");
    assert_eq!(
        sink.video_tcs,
        vec![b(1), b(2)],
        "one grid slot per catch-up"
    );
}

// ---------------------------------------------------------------------------
// (b) A sustained 3 ms iteration cost at 30-fps content keeps lag_slots <= 1
// over 300 boundaries and emits 300 frames — the fix must NOT throttle the
// healthy (iter_cost < interval) case.
// ---------------------------------------------------------------------------

#[test]
fn three_ms_iteration_cost_keeps_lag_bounded_over_300_boundaries() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    // 30-fps content: frame j due at b(j+1).
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..400i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let mut sink = RecordingSink::default();

    // Each decoder pull costs 3 ms (30_000 × 100 ns), advanced INSIDE the pull.
    let pull = |frames: &std::cell::RefCell<std::collections::VecDeque<PacedFrame>>| {
        clk2.advance(30_000);
        frames.borrow_mut().pop_front()
    };

    let mut emits = 0;
    let mut max_lag = 0i64;
    let mut guard = 0;
    while emits < 300 && guard < 5000 {
        guard += 1;
        let out = pacer.service(|| pull(&frames), &mut sink);
        match out {
            ServiceOutcome::Wait { until_100ns }
            | ServiceOutcome::Reanchored { until_100ns, .. } => {
                clk.set(until_100ns.max(clk.get())); // "sleep" to the boundary
            }
            ServiceOutcome::Emitted => {
                emits += 1;
                max_lag = max_lag.max(pacer.stats().lag_slots);
            }
            ServiceOutcome::Repeated | ServiceOutcome::Starved => {}
        }
    }

    assert_eq!(emits, 300, "all 300 frames emitted");
    assert_eq!(
        pacer.stats().resyncs,
        0,
        "no resync/re-anchor in the healthy case"
    );
    assert!(
        max_lag <= 1,
        "lag stays within one slot with a 3 ms iteration cost, got {max_lag}"
    );
    assert!(
        pacer.stats().iter_p99_us >= 3_000 && pacer.stats().iter_p99_us <= 7_000,
        "iter p99 reflects the ~3 ms pull cost, got {}",
        pacer.stats().iter_p99_us
    );
    assert!(pacer.max_lag_slots() <= 1, "max lag <= 1 over the run");
}

// ---------------------------------------------------------------------------
// (c) Change 2 — lag > 8 sustained > 1 s with frames buffered re-anchors
// EXACTLY once, skipping no content frame; subsequent stamps <= now and one
// slot apart.
// ---------------------------------------------------------------------------

#[test]
fn sustained_lag_over_bound_reanchors_once_without_skipping_content() {
    let (mut pacer, clk) = anchored_pacer(); // wall_start=B1, next_boundary=b(1)
    let frames = std::cell::RefCell::new(std::collections::VecDeque::new());
    for j in 0..40i64 {
        frames.borrow_mut().push_back(frame_due_at(b(j + 1)));
    }
    let mut sink = RecordingSink::default();

    // Call 1: 19 slots late (lag > 8) — catch up one slot, arm the re-anchor
    // timer; NOT sustained yet, so no re-anchor.
    clk.set(b(20));
    let o1 = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    assert_eq!(o1, ServiceOutcome::Emitted);
    assert_eq!(
        pacer.stats().resyncs,
        0,
        "one late call is not yet sustained"
    );
    assert_eq!(sink.pts[0], 0, "first emitted content frame is frame 0");

    // Call 2: still far behind AND > 1 s of continuous lag -> RE-ANCHOR once.
    clk.set(b(20) + 10_000_001); // 1 s + 1 tick later
    let o2 = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    let until = match o2 {
        ServiceOutcome::Reanchored {
            lag_slots,
            until_100ns,
        } => {
            assert!(
                lag_slots > 8,
                "re-anchor reports the offending lag: {lag_slots}"
            );
            until_100ns
        }
        other => panic!("expected Reanchored, got {other:?}"),
    };
    assert_eq!(pacer.stats().resyncs, 1, "exactly one re-anchor");
    // The re-anchor must NOT emit or consume the buffered frame (no burst).
    assert_eq!(sink.video_tcs.len(), 1, "re-anchor emits nothing itself");

    // Call 3 at the new boundary: the PRESERVED buffered frame (content frame 1)
    // is emitted next — no skip — stamped on-grid <= now.
    clk.set(until);
    let o3 = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    assert_eq!(o3, ServiceOutcome::Emitted);
    assert_eq!(
        sink.pts,
        vec![0, (b(2) - B1) * 100],
        "content continues from the buffered frame — no frame skipped"
    );
    assert_eq!(
        *sink.video_tcs.last().unwrap(),
        until,
        "stamped at the new boundary"
    );
    assert!(
        *sink.video_tcs.last().unwrap() <= until,
        "never future-dated"
    );
    assert_eq!(pacer.stats().dropped, 0, "re-anchor drops no content");
    assert_eq!(
        sp_core::genlock::floor_boundary_100ns(until, 30),
        until,
        "re-anchor boundary is on the grid"
    );
}

// ---------------------------------------------------------------------------
// (d) Change 3 — late_frames counts an emit 3 ms late but NOT one 1 ms late
// (LATE_THRESHOLD_100NS = 2 ms).
// ---------------------------------------------------------------------------

#[test]
fn late_frames_uses_the_two_millisecond_threshold() {
    // 1 ms late: under the 2 ms threshold -> not counted.
    let (mut pacer, clk) = anchored_pacer();
    let mut sink = RecordingSink::default();
    let mut f1 = Some(frame_due_at(b(1)));
    clk.set(b(1) + 10_000); // 1 ms past b(1)
    pacer.service(|| f1.take(), &mut sink);
    assert_eq!(
        pacer.stats().late_frames,
        0,
        "1 ms late is under the 2 ms late threshold"
    );

    // 3 ms late: over the threshold -> counted.
    let mut f2 = Some(frame_due_at(b(2)));
    clk.set(b(2) + 30_000); // 3 ms past b(2)
    pacer.service(|| f2.take(), &mut sink);
    assert_eq!(
        pacer.stats().late_frames,
        1,
        "3 ms late crosses the 2 ms late threshold"
    );
}

// ---------------------------------------------------------------------------
// (e) Change 3 — PacingStats serialises lag_slots and iter_p99_us.
// ---------------------------------------------------------------------------

#[test]
fn pacing_stats_serialises_lag_slots_and_iter_p99() {
    let (mut pacer, clk) = anchored_pacer();
    let clk2 = clk.clone();
    let mut first = Some(frame_due_at(b(1)));
    clk.set(b(1));
    pacer.service(
        || {
            clk2.advance(20_000); // 2 ms "decode"
            first.take()
        },
        &mut RecordingSink::default(),
    );

    let stats = pacer.stats();
    let json = serde_json::to_value(&stats).unwrap();
    for field in ["lag_slots", "iter_p99_us"] {
        assert!(
            json.get(field).is_some(),
            "PacingStats must serialise the new field {field}"
        );
    }
    // Round-trips back to an equal value.
    let back: crate::playback::ndi_health::PacingStats = serde_json::from_value(json).unwrap();
    assert_eq!(back, stats);
    assert!(
        pacer.iter_p50_us() >= 1_000,
        "iter p50 reflects the decode cost"
    );
}

// ---------------------------------------------------------------------------
// lag_slots_100ns gauge (pure) — a division-based whole-slot lag count.
// ---------------------------------------------------------------------------

#[test]
fn lag_slots_100ns_counts_whole_slots_behind() {
    use sp_core::genlock::{floor_boundary_100ns, lag_slots_100ns};
    // On the boundary -> 0.
    assert_eq!(lag_slots_100ns(b(5), b(5), 30), 0);
    // One slot past -> 1.
    assert_eq!(lag_slots_100ns(b(5), b(6), 30), 1);
    // Ten slots past -> 10.
    assert_eq!(lag_slots_100ns(b(5), b(15), 30), 10);
    // floor_now before the boundary -> 0 (never negative).
    assert_eq!(lag_slots_100ns(b(5), b(4), 30), 0);
    // fps <= 0 -> 0 (guarded divisor).
    assert_eq!(lag_slots_100ns(b(5), b(15), 0), 0);
    // A concrete wall reading floored first.
    let now = b(9) + 12_345;
    assert_eq!(lag_slots_100ns(b(1), floor_boundary_100ns(now, 30), 30), 8);
}
