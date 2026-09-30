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
//! A fleet date step RELABELS time, it does not move content (#224 part 2,
//! design record 5899388193). The wall's step probe follows a step of either
//! sign at the boundary it lands; the whole slots N move only the labels, so
//! the pacer's timeline moves by the remainder r (0 ≤ r ≤ one slot, forward):
//! no catch-up burst after a forward step, no pause after a backward one, the
//! content one frame per boundary throughout. A REAL stall still advances the
//! timeline by the real gap and is still caught up. Every pin was derived with
//! a scratch Python model of this harness (the wall, the split, the pacer).

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::genlock::strict_next_boundary_100ns;
use sp_ndi::AudioFrame;

use super::ahead_frame;
use crate::playback::fleet_shift::{FleetShift, shift_100ns, wire_stamp_100ns};
use crate::playback::pacer::{PacedFrame, PacedSink, Pacer, ServiceOutcome, plan_sleep_100ns};
use crate::playback::wallclock::{ClockSource, VirtualClock, WallClock};

/// A 40 ms preemption between the monotonic and the UTC read.
const PREEMPT_40MS_NS: u64 = 40_000_000;
/// A dantesync 1.12.0 nightly date step backward: −1.5 s in 100-ns units.
const STEP_BACK_1_5_S: i64 = -15_000_000;
/// The 20:58Z controlled step (+260.3 ms): 7 whole slots + r = 26.97 ms.
const STEP_PLUS_260_3_MS: i64 = 2_603_000;
/// One 30-fps grid slot in ns: 333 333 or 333 334 × 100 ns (exact-rational grid).
const SLOT_NS: std::ops::RangeInclusive<u128> = 33_333_300..=33_333_400;
/// A real 200 ms stall of the paced thread (6 whole slots, no clock step).
const STALL_200MS_NS: u64 = 200_000_000;

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
    /// Emits whose audio stamp is not their video stamp (#224: a boundary's
    /// audio block belongs to that boundary's timeline instant).
    audio_off_boundary: u64,
    /// The emit count (= serviced boundaries) of every boundary serviced at
    /// the same virtual instant as the one before it: a catch-up burst.
    burst_services: Vec<u64>,
    /// `(emit count, gap)` of every gap between two serviced boundaries that
    /// is not exactly one grid slot (#224 part 2).
    odd_gaps: Vec<(u64, Duration)>,
    /// Emitted frames whose pts is not the NEXT 30-fps frame after the
    /// previous one's: content skipped or repeated (#224 part 2).
    non_contiguous_frames: u64,
    last_pts_100ns: Option<i64>,
    /// When set, the relabel registry the pacer's wall follows: each emit's
    /// WIRE stamp is `floor(stamp + D(K_F))` as the submit edge puts it on.
    fleet: Option<Arc<FleetShift>>,
    last_wire: Option<i64>,
    /// `(emit count, Δ)` of every wire stamp that is not the next slot.
    wire_jumps: Vec<(u64, i64)>,
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
            if gap.is_zero() {
                self.burst_services.push(self.emits);
            }
            if !SLOT_NS.contains(&gap.as_nanos()) {
                self.odd_gaps.push((self.emits, gap));
            }
        }
        self.last_service_at = Some(at);
    }
}

impl PacedSink for StampSink {
    fn emit(
        &mut self,
        video: &PacedFrame,
        _audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        if audio_tc_100ns != video_tc_100ns {
            self.audio_off_boundary += 1;
        }
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
        let pts = video.pts_ns / 100;
        if self
            .last_pts_100ns
            .is_some_and(|prev| pts != strict_next_boundary_100ns(prev, 30))
        {
            self.non_contiguous_frames += 1;
        }
        self.last_pts_100ns = Some(pts);
        if let Some(fleet) = &self.fleet {
            let wire = wire_stamp_100ns(video_tc_100ns, fleet.slots());
            if let Some(prev) = self.last_wire
                && wire != strict_next_boundary_100ns(prev, 30)
            {
                self.wire_jumps.push((self.emits + 1, wire - prev));
            }
            self.last_wire = Some(wire);
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
/// tick (so a UTC step or a preempted read lands on a chosen resample). The
/// wall follows date steps through a registry of its own.
fn run_paced_with(
    clk: &Arc<VirtualClock>,
    boundaries: u64,
    before_tick: impl FnMut(u64),
) -> (Pacer, StampSink) {
    run_paced_on(clk, Arc::default(), boundaries, before_tick)
}

/// [`run_paced_with`] on the relabel registry `fleet`; the sink records each
/// emit's wire stamp under it (#224 part 2).
fn run_paced_on(
    clk: &Arc<VirtualClock>,
    fleet: Arc<FleetShift>,
    boundaries: u64,
    mut before_tick: impl FnMut(u64),
) -> (Pacer, StampSink) {
    let wall = WallClock::with_fleet(Box::new(clk.clone()), fleet.clone());
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    pacer.anchor();
    let next = Cell::new(0i64);
    let mut sink = StampSink {
        fleet: Some(fleet),
        ..StampSink::default()
    };
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

/// The step `step_100ns` lands after the 150th boundary (with a real stall of
/// `stall_ns` first, 0 for none), 400 boundaries in all, on a registry of its
/// own.
fn run_step(step_100ns: i64, stall_ns: u64) -> (Pacer, StampSink, Arc<VirtualClock>) {
    let clk = VirtualClock::new(0);
    let (pacer, sink) = run_paced_on(&clk, Arc::default(), 400, |ticks| {
        if ticks == 150 {
            clk.advance_ns(stall_ns);
            clk.step_utc(step_100ns);
        }
    });
    (pacer, sink, clk)
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
    assert_eq!(s.wall_anchor_steps_followed, 0);
    assert_eq!(
        sink.audio_off_boundary, 0,
        "every audio block on its boundary"
    );
    // 1 construction read + 100 resamples, of which 50 needed a second read,
    // + one quiet step-probe read per serviced boundary (#224).
    assert_eq!(clk.reads(), 1 + 100 + 50 + 10_000);
}

#[test]
fn pacing_stats_carry_the_pacer_wall_anchor_telemetry() {
    let clk = VirtualClock::new(0);
    // Construction: every attempt preempted by 400 µs, so one wide bracket and
    // an anchor 200 µs ahead of truth.
    clk.delay_next_reads(&[400_000; 8]);
    let mut pacer = Pacer::with_wallclock(30, true, WallClock::new(Box::new(clk.clone())));
    // A genuine +5 ms UTC step, 4.8 ms from the wall: the step probe of the
    // first boundary follows it whole (#224); nothing is slewed.
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
            s.wall_anchor_slewed_us,
            s.wall_anchor_steps_followed,
            s.wall_anchor_last_step_us
        ),
        (4_800, 1, 0, 1, 4_800)
    );
    // Under one slot: nothing relabelled, the whole 4.8 ms is the remainder.
    assert_eq!(
        (s.fleet_shift_slots, s.last_regrid_remainder_us),
        (0, 4_800)
    );
}

#[test]
fn pacing_stats_carry_a_followed_utc_step() {
    // #224: a +50 ms fleet date step is followed in one event by the step
    // probe of the first boundary and reported through the pacer's own stats.
    // #224 part 2: 1 whole slot relabelled, r = 16.67 ms.
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
    assert_eq!(s.wall_anchor_slewed_us, 0, "nothing slewed: followed whole");
    assert_eq!(
        (s.fleet_shift_slots, s.last_regrid_remainder_us),
        (1, 16_666),
        "N = 1, r = 16.67 ms"
    );
    assert_eq!(
        pacer.now_100ns(),
        clk.truth_100ns() - shift_100ns(1),
        "on the stepped UTC less the relabel"
    );
}

#[test]
fn a_minus_1_5_s_step_never_pauses_the_output_and_keeps_one_frame_per_boundary() {
    // #224 part 2 (design record 5899388193): the nightly −1.5 s step lands at
    // tick 150. It is exactly −45 whole slots, r = 0: the probe of that tick
    // relabels it, and the pacer's timeline does not move at all. Before, the
    // wall froze for the whole 1.5 s — ONE output pause of 1.5 s + one slot.
    let (pacer, sink, clk) = run_step(STEP_BACK_1_5_S, 0);
    let s = pacer.stats();
    assert_eq!(sink.emits, 400);
    assert_eq!(sink.long_pauses, 0, "no output pause");
    assert!(
        SLOT_NS.contains(&sink.max_service_gap.as_nanos()),
        "every gap is one slot, got {:?}",
        sink.max_service_gap
    );
    assert!(
        sink.odd_gaps.is_empty(),
        "r = 0: not even one short interval: {:?}",
        sink.odd_gaps
    );
    assert!(sink.burst_services.is_empty(), "no catch-up burst");
    assert_eq!(s.relatches, 0, "the timeline never went back");
    assert_eq!(sink.non_advancing_stamps, 0, "no slot is ever re-served");
    assert_eq!(sink.non_contiguous_stamps, 0, "no slot is skipped");
    assert_eq!(sink.non_contiguous_frames, 0, "one frame per boundary");
    assert_eq!((s.resyncs, s.dropped, s.repeats), (0, 0, 0));
    assert_eq!(s.av_corrections, 0, "the audio never leaves the picture");
    assert_eq!(pacer.audio_stats().underruns, 0);
    assert_eq!(
        sink.audio_off_boundary, 0,
        "every audio block on its boundary"
    );
    assert_eq!(
        (
            s.wall_anchor_steps_followed,
            s.wall_anchor_holds_followed,
            s.wall_anchor_last_step_us,
            s.wall_anchor_last_hold_us
        ),
        (1, 0, -1_500_000, 0),
        "one followed step, and no hold"
    );
    assert_eq!(s.wall_anchor_slewed_us, 0, "nothing slewed");
    assert_eq!(
        pacer.now_100ns(),
        clk.truth_100ns() - STEP_BACK_1_5_S,
        "the timeline runs 1.5 s ahead of the new labels"
    );
}

#[test]
fn a_plus_90_ms_step_is_relabelled_at_its_boundary_with_no_catch_up_burst() {
    // #224: the 02:00Z nightly step (~+90 ms) lands at tick 150, and the step
    // probe of that tick follows it. #224 part 2: 2 whole slots relabelled,
    // the timeline moves r = 23.3 ms, so the next boundary comes 23.3 ms
    // early — never the two back-to-back catch-up emits (ticks 151 and 152)
    // it used to take.
    let (pacer, sink, _clk) = run_step(900_000, 0);
    let s = pacer.stats();
    assert_eq!(sink.emits, 400);
    assert_eq!(sink.burst_services, Vec::<u64>::new(), "no catch-up burst");
    assert_eq!(
        sink.odd_gaps,
        vec![(151, Duration::from_nanos(10_000_000))],
        "ONE interval shrinks by r: 33.33 − 23.33 ms"
    );
    assert_eq!(
        sink.audio_off_boundary, 0,
        "every audio block on its boundary"
    );
    assert_eq!(sink.non_advancing_stamps, 0);
    assert_eq!(sink.non_contiguous_stamps, 0, "no slot is skipped");
    assert_eq!(sink.non_contiguous_frames, 0, "one frame per boundary");
    assert_eq!((s.relatches, s.resyncs, s.dropped), (0, 0, 0));
    assert_eq!(
        (
            s.wall_anchor_steps_followed,
            s.wall_anchor_last_step_us,
            s.wall_anchor_holds_followed,
            s.wall_anchor_slewed_us
        ),
        (1, 90_000, 0, 0),
        "one forward step, followed whole"
    );
    assert_eq!(
        sink.wire_jumps,
        vec![(151, 1_000_000)],
        "the wire stamps jump 3 slots once: 1 + the 2 relabelled"
    );
}

#[test]
fn a_plus_260_3_ms_step_relabels_the_wire_and_keeps_content_one_frame_per_boundary() {
    // The 20:58Z controlled step (FINDING 5898834252): +260.3 ms = 7 slots +
    // r = 26.97 ms. The internal stamps stay contiguous, the content goes out
    // one frame per boundary, no boundary is serviced in a burst and no gap is
    // over one slot; only the WIRE jumps: 8 slots once (1 + the 7 relabelled).
    // Before, the pacer caught the 7 slots up back to back (the relock camera-
    // box traced, and the VBAN burst).
    let (pacer, sink, clk) = run_step(STEP_PLUS_260_3_MS, 0);
    let s = pacer.stats();
    assert_eq!(sink.emits, 400);
    assert!(sink.burst_services.is_empty(), "no catch-up burst");
    assert!(
        sink.max_service_gap.as_nanos() <= *SLOT_NS.end(),
        "no service gap over one slot: {:?}",
        sink.max_service_gap
    );
    assert_eq!(
        sink.odd_gaps,
        vec![(151, Duration::from_nanos(6_366_700))],
        "ONE interval shrinks by r: 33.33 − 26.97 ms"
    );
    assert_eq!(sink.non_contiguous_stamps, 0, "internal stamps contiguous");
    assert_eq!(sink.non_contiguous_frames, 0, "one frame per boundary");
    assert_eq!((s.relatches, s.resyncs, s.dropped, s.repeats), (0, 0, 0, 0));
    assert_eq!(
        sink.wire_jumps,
        vec![(151, 2_666_666)],
        "the wire jumps 8 slots once"
    );
    assert_eq!(
        (s.fleet_shift_slots, s.last_regrid_remainder_us),
        (7, 26_966)
    );
    assert_eq!(pacer.now_100ns(), clk.truth_100ns() - shift_100ns(7));
}

#[test]
fn a_minus_19_8_ms_step_sends_the_same_wire_stamp_twice_and_never_pauses() {
    // −19.8 ms = −1 slot + r = 13.53 ms (the mid-day date-master restart):
    // the next boundary comes 13.53 ms early and the wire goes back ONE slot,
    // so that boundary's stamp repeats the previous one (camera-box: STEADY
    // presents both, `stamp_dup` +1, no relock). No pause.
    let (pacer, sink, _clk) = run_step(-198_000, 0);
    let s = pacer.stats();
    assert!(sink.burst_services.is_empty());
    assert_eq!(sink.long_pauses, 0);
    assert_eq!(sink.odd_gaps, vec![(151, Duration::from_nanos(19_800_000))]);
    assert_eq!(sink.non_contiguous_frames, 0, "one frame per boundary");
    assert_eq!(sink.wire_jumps, vec![(151, 0)], "the same wire stamp twice");
    assert_eq!(
        (s.fleet_shift_slots, s.last_regrid_remainder_us),
        (-1, 13_533)
    );
    assert_eq!(s.wall_anchor_holds_followed, 0);
}

#[test]
fn a_real_200_ms_stall_still_catches_up_6_boundaries() {
    // Stall vs step is told apart by structure (design record 5899388193):
    // the timeline carries the REAL gap, so the #147 catch-up still services
    // the 6 boundaries the stall skipped (151..156) at once.
    let (pacer, sink, _clk) = run_step(0, STALL_200MS_NS);
    let s = pacer.stats();
    assert_eq!(sink.burst_services, vec![152, 153, 154, 155, 156]);
    assert_eq!(
        sink.odd_gaps.first(),
        Some(&(151, Duration::from_nanos(STALL_200MS_NS))),
        "the stall itself"
    );
    assert_eq!(sink.non_contiguous_stamps, 0, "caught up, never skipped");
    assert_eq!((s.resyncs, s.wall_anchor_steps_followed), (0, 0));
    assert_eq!(s.fleet_shift_slots, 0);
}

#[test]
fn a_stall_and_a_step_in_the_same_tick_catch_up_only_the_stall() {
    // A 200 ms stall AND the +260.3 ms step before the same tick: the stall's
    // 6 boundaries are caught up, the step's 7 slots are relabelled, and only
    // r = 26.97 ms of it reaches the timeline (the boundary after the
    // catch-up comes that much early), never 13 back-to-back emits.
    let (pacer, sink, _clk) = run_step(STEP_PLUS_260_3_MS, STALL_200MS_NS);
    let s = pacer.stats();
    assert_eq!(sink.burst_services, vec![152, 153, 154, 155, 156]);
    assert_eq!(
        sink.odd_gaps.last(),
        Some(&(157, Duration::from_nanos(6_366_700)))
    );
    assert_eq!(sink.non_contiguous_stamps, 0);
    assert_eq!(sink.non_contiguous_frames, 0);
    assert_eq!(s.resyncs, 0);
    assert_eq!(s.fleet_shift_slots, 7);
    assert_eq!(sink.wire_jumps, vec![(151, 2_666_666)]);
}

#[test]
fn pacing_stats_carry_the_step_probe_telemetry() {
    // #224: the first probe after a +90 ms step is preempted (a wide read,
    // rejected), the next boundary's follows it: `/api/v1/ndi/health` and the
    // `ndi: genlock` line read the rejection and the detect-to-follow time.
    let clk = VirtualClock::new(0);
    let mut pacer = Pacer::with_wallclock(30, true, WallClock::new(Box::new(clk.clone())));
    clk.step_utc(900_000);
    clk.delay_next_reads(&[400_000]);
    for _ in 0..2 {
        clk.advance_ns(33_333_300);
        pacer.tick_wall();
    }
    let s = pacer.stats();
    assert_eq!(
        (
            s.wall_anchor_probes_rejected,
            s.wall_anchor_detect_to_follow_us,
            s.wall_anchor_steps_followed,
            s.wall_anchor_last_step_us
        ),
        (1, 33_533, 1, 90_000),
        "one rejected probe, then the follow one frame + 200 µs after it"
    );
}
