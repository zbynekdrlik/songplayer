//! #147: the WallClock re-anchor must not step the genlock wall. This is the
//! pacer-level acceptance.
//!
//! The box relatched 6× (SP-slow `relatches` 0 → 6, `av_corrections` 1 → 200)
//! in one A/V take. A re-anchor paired a stale `Instant` with a later UTC read,
//! so the pacer's wall jumped forward by the preemption, and the next clean
//! re-anchor stepped it back across boundaries already emitted.
//!
//! These tests drive the PURE [`Pacer`] exactly the way
//! `pipeline_paced::run_paced` does, but in virtual time:
//! - it sleeps to the next boundary through [`plan_sleep_100ns`], with no sleep
//!   when the plan says relatch;
//! - it services the boundary;
//! - it sleeps to a `Wait` target;
//! - it ticks the wall once per serviced boundary.
//!
//! The wall is a real [`WallClock`] over a [`VirtualClock`] whose every other
//! resample meets a 40 ms preemption between its monotonic and UTC reads. The
//! audio is exactly paired with the video (`ahead_frame(j, 0)`), so any A/V
//! correction can only come from the clock.
//!
//! Nested under `pacer_tests_av_align.rs` so it reuses its media-encoded frame
//! helper.
//!
//! A confirmed BACKWARD fleet date step (dantesync 1.12.0: once a night, up to
//! ~−1.5 s) is followed as ONE hold of the pacer's wall (#147, design record
//! 5850063723). The pacer then waits through the frozen wall and resumes on
//! the very next slot.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::genlock::strict_next_boundary_100ns;
use sp_ndi::AudioFrame;

use super::ahead_frame;
use crate::playback::pacer::{PacedFrame, PacedSink, Pacer, ServiceOutcome, plan_sleep_100ns};
use crate::playback::wallclock::{ClockSource, VirtualClock, WallClock};

/// A 40 ms preemption between the monotonic and the UTC read.
const PREEMPT_40MS_NS: u64 = 40_000_000;
/// A dantesync 1.12.0 nightly date step backward: −1.5 s in 100-ns units.
const STEP_BACK_1_5_S: i64 = -15_000_000;
/// One 30-fps grid slot in ns: 333 333 or 333 334 × 100 ns (exact-rational grid).
const SLOT_NS: std::ops::RangeInclusive<u128> = 33_333_300..=33_333_400;

/// Counts emits and any video stamp that fails to advance (a re-served slot)
/// or skips a slot. It also keeps the longest virtual-monotonic gap between
/// two serviced boundaries (the run loop notes each one).
#[derive(Default)]
struct StampSink {
    emits: u64,
    last_video_tc: Option<i64>,
    non_advancing_stamps: u64,
    /// Stamps that are not the NEXT 30-fps grid slot after the previous one.
    non_contiguous_stamps: u64,
    last_service_at: Option<Instant>,
    max_service_gap: Duration,
    /// Gaps between two serviced boundaries longer than [`LONG_PAUSE`].
    long_pauses: u64,
}

/// A gap between two serviced boundaries this long is a real output pause
/// (three slots; a 1 ms arming hold only stretches one slot to ~34 ms).
const LONG_PAUSE: Duration = Duration::from_millis(100);

impl StampSink {
    /// The run loop serviced a boundary at virtual monotonic time `at`.
    fn note_service_at(&mut self, at: Instant) {
        if let Some(prev) = self.last_service_at {
            let gap = at.saturating_duration_since(prev);
            self.max_service_gap = self.max_service_gap.max(gap);
            if gap > LONG_PAUSE {
                self.long_pauses += 1;
            }
        }
        self.last_service_at = Some(at);
    }
}

impl PacedSink for StampSink {
    fn emit(
        &mut self,
        _video: &PacedFrame,
        _audio: &[AudioFrame],
        video_tc_100ns: i64,
        _audio_tc_100ns: i64,
    ) {
        let advanced = self.last_video_tc.is_none_or(|prev| video_tc_100ns > prev);
        if !advanced {
            self.non_advancing_stamps += 1;
        }
        let contiguous = self
            .last_video_tc
            .is_none_or(|prev| video_tc_100ns == strict_next_boundary_100ns(prev, 30));
        if !contiguous {
            self.non_contiguous_stamps += 1;
        }
        self.last_video_tc = Some(video_tc_100ns);
        self.emits += 1;
    }
}

/// `pipeline_paced::sleep_to_boundary` in virtual time: advance the monotonic
/// clock by the planned sleep, or not at all when the plan says relatch (a
/// backward wall step: the loop re-latches instead).
fn sleep_to(clk: &VirtualClock, pacer: &Pacer, until_100ns: i64) {
    let plan = plan_sleep_100ns(pacer.now_100ns(), until_100ns, pacer.interval_100ns());
    if !plan.relatch {
        clk.advance_ns(plan.sleep_100ns as u64 * 100);
    }
}

/// Service `boundaries` paced boundaries in virtual time. When `preempt` is
/// set, resamples 1, 3, 5, … (ticks 100, 300, …) meet a 40 ms preemption;
/// the even ones are clean.
fn run_paced(clk: &Arc<VirtualClock>, boundaries: u64, preempt: bool) -> (Pacer, StampSink) {
    run_paced_with(clk, boundaries, |ticks| {
        if preempt && ticks % 200 == 100 {
            clk.delay_next_reads(&[PREEMPT_40MS_NS]);
        }
    })
}

/// Service `boundaries` paced boundaries in virtual time, calling
/// `before_tick(ticks)` after each serviced boundary, right before the wall
/// tick (so a UTC step or a preempted read lands on a chosen resample).
fn run_paced_with(
    clk: &Arc<VirtualClock>,
    boundaries: u64,
    mut before_tick: impl FnMut(u64),
) -> (Pacer, StampSink) {
    let mut pacer = Pacer::with_wallclock(30, true, WallClock::new(Box::new(clk.clone())));
    pacer.anchor();
    let next = Cell::new(0i64);
    let mut sink = StampSink::default();
    let mut ticks = 0u64;
    let mut iterations = 0u64;
    while ticks < boundaries {
        iterations += 1;
        assert!(
            iterations < 4 * boundaries,
            "the simulated paced loop stopped making progress"
        );
        let target = pacer.next_boundary_100ns();
        sleep_to(clk, &pacer, target);
        let outcome = pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(ahead_frame(j, 0))
            },
            &mut sink,
        );
        match outcome {
            ServiceOutcome::Wait { until_100ns }
            | ServiceOutcome::Reanchored { until_100ns, .. } => sleep_to(clk, &pacer, until_100ns),
            ServiceOutcome::Emitted | ServiceOutcome::Repeated | ServiceOutcome::Starved => {
                ticks += 1;
                sink.note_service_at(clk.now_monotonic());
                before_tick(ticks);
                pacer.tick_wall();
            }
        }
    }
    (pacer, sink)
}

#[test]
fn a_preempted_re_anchor_causes_no_relatch_and_no_av_correction_over_10000_boundaries() {
    let clk = VirtualClock::new(0);
    let (pacer, sink) = run_paced(&clk, 10_000, true);
    let s = pacer.stats();
    assert_eq!(sink.emits, 10_000);
    assert_eq!(
        s.relatches, 0,
        "a preempted anchor sample must never step the pacer's wall back"
    );
    assert_eq!(s.av_corrections, 0, "the audio never leaves the picture");
    assert_eq!(s.av_corrected_samples, 0);
    assert_eq!(
        sink.non_advancing_stamps, 0,
        "no grid slot is ever re-served"
    );
    assert_eq!(s.resyncs, 0);
    assert_eq!(pacer.audio_stats().underruns, 0);
    // Every preempted read was re-tried and outvoted: no wide bracket, and no
    // resample measured any offset at all.
    assert_eq!(s.wall_anchor_wide_brackets, 0);
    assert_eq!(s.wall_anchor_max_step_us, 0);
    assert_eq!(s.wall_anchor_slewed_us, 0);
    // 1 construction read + 100 resamples, of which 50 needed a second read.
    assert_eq!(clk.reads(), 1 + 100 + 50);
}

#[test]
fn pacing_stats_carry_the_pacer_wall_anchor_telemetry() {
    let clk = VirtualClock::new(0);
    // Construction: every attempt preempted by 400 µs, so one wide bracket and
    // an anchor 200 µs ahead of truth.
    clk.delay_next_reads(&[400_000; 8]);
    let mut pacer = Pacer::with_wallclock(30, true, WallClock::new(Box::new(clk.clone())));
    // A genuine +5 ms UTC step, measured as 4.8 ms at the first resample; 1 ms
    // is slewed in.
    clk.step_utc(50_000);
    for _ in 0..100 {
        clk.advance_ns(33_333_300);
        pacer.tick_wall();
    }
    let s = pacer.stats();
    assert_eq!(
        (
            s.wall_anchor_max_step_us,
            s.wall_anchor_wide_brackets,
            s.wall_anchor_slewed_us
        ),
        (4_800, 1, 1_000)
    );
}

#[test]
fn pacing_stats_carry_a_followed_utc_step() {
    // #147 (b): a +50 ms fleet date step, confirmed by the second resample, is
    // followed in one event and reported through the pacer's own stats.
    let clk = VirtualClock::new(0);
    let mut pacer = Pacer::with_wallclock(30, true, WallClock::new(Box::new(clk.clone())));
    clk.step_utc(500_000);
    for _ in 0..200 {
        clk.advance_ns(33_333_300);
        pacer.tick_wall();
    }
    let s = pacer.stats();
    assert_eq!(s.wall_anchor_steps_followed, 1);
    assert_eq!(s.wall_anchor_last_step_us, 50_000);
    assert_eq!(s.wall_anchor_holds_followed, 0, "a forward step is no hold");
    assert_eq!(
        s.wall_anchor_slewed_us, 1_000,
        "only the arming resample slewed"
    );
    assert_eq!(pacer.now_100ns(), clk.truth_100ns(), "on the stepped UTC");
}

#[test]
fn a_followed_minus_1_5_s_step_pauses_the_output_once_with_no_relatch_or_av_correction() {
    // #147 (design record 5850063723): the step lands at tick 150. Resample 2
    // (tick 200) arms it with a 1 ms hold, and resample 3 (tick 300) follows
    // the rest as ONE 1.499 s hold of the pacer's wall.
    let clk = VirtualClock::new(0);
    let (pacer, sink) = run_paced_with(&clk, 400, |ticks| {
        if ticks == 150 {
            clk.step_utc(STEP_BACK_1_5_S);
        }
    });
    let s = pacer.stats();
    assert_eq!(sink.emits, 400);
    assert_eq!(s.relatches, 0, "the wall froze, it never stepped back");
    assert_eq!(s.av_corrections, 0, "the audio never leaves the picture");
    assert_eq!(s.av_corrected_samples, 0);
    assert_eq!(sink.non_advancing_stamps, 0, "no slot is ever re-served");
    // The frozen wall resumes from the value it froze at, so the next boundary
    // is the next slot: no slot is skipped, and the pacer never lags, so it
    // never resyncs either.
    assert_eq!(sink.non_contiguous_stamps, 0, "no slot is skipped");
    assert_eq!(s.resyncs, 0);
    assert_eq!(pacer.audio_stats().underruns, 0);
    // The output paused ONCE, for the followed hold plus the next slot.
    assert_eq!(sink.long_pauses, 1, "exactly one output pause");
    let pause_ns = sink.max_service_gap.as_nanos();
    assert!(
        pause_ns
            .checked_sub(1_499_000_000)
            .is_some_and(|slot| SLOT_NS.contains(&slot)),
        "the longest gap between two boundaries must be the 1.499 s hold + one slot, got {pause_ns} ns"
    );
    assert_eq!(
        (
            s.wall_anchor_steps_followed,
            s.wall_anchor_holds_followed,
            s.wall_anchor_last_step_us,
            s.wall_anchor_last_hold_us
        ),
        (1, 1, -1_500_000, 1_499_000),
        "one followed step, and it was one hold"
    );
    assert_eq!(s.wall_anchor_max_step_us, 1_500_000);
    assert_eq!(
        s.wall_anchor_slewed_us, 1_000,
        "only the arming hold slewed"
    );
    assert_eq!(pacer.now_100ns(), clk.truth_100ns(), "on the UTC line");
}
