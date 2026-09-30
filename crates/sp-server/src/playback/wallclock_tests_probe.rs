//! #224 (design record 5890605448, Approach 1): the WallClock follows a
//! dantesync date step at the boundary it lands, not two 100-frame resamples
//! (3.3–6.7 s) later.
//!
//! Every tick takes ONE bracketed read (the step probe) and measures it
//! against the line the wall runs on. Over 2 ms from a narrow bracket, a full
//! anchor sample is taken in the same tick; when both agree within 1 ms the
//! step is followed in ONE event. Since #224 part 2 that event RELABELS: the
//! UTC anchor takes the whole step S, the wall's timeline moves N = ⌊S/slot⌋
//! whole slots behind its labels, and the timeline itself moves only by the
//! remainder r (0 ≤ r ≤ one slot, forward) — never a hold of the step. A wide
//! probe, or a confirmation that is wide or reads another step, is rejected,
//! and nothing is armed, so the next boundary still follows a real step at
//! once. The 100-frame resample keeps slewing everything within 2 ms.
//!
//! A WallClock over the [`VirtualClock`], exact values (derived with a scratch
//! Python model of the wall + the split). The pure rule (`decide_step_probe`)
//! is pinned in `wallclock_tests_probe_rule.rs`.

use std::sync::Arc;

use super::*;
use crate::playback::fleet_shift::shift_100ns;

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;
/// 1 ms in 100-ns units.
const MS: i64 = 10_000;

/// Advance one frame and tick; return the wall just before and just after
/// the tick.
fn tick_once(wall: &mut WallClock, clk: &VirtualClock) -> (i64, i64) {
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    (before, wall.now_100ns())
}

/// `n` frames, one tick each.
fn ticks(wall: &mut WallClock, clk: &VirtualClock, n: u64) {
    for _ in 0..n {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
}

/// A fresh wall on the true UTC line, ten quiet boundaries in.
fn settled() -> (Arc<VirtualClock>, WallClock) {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 10);
    (clk, wall)
}

#[test]
fn a_plus_90_ms_and_a_plus_700_ms_step_are_relabelled_at_the_boundary_they_land() {
    // The 02:00Z nightly step (89.7 ms) and a full-day step (~0.7 s): 2 and
    // 21 whole slots; the timeline moves only the remainder (23.3 ms / 0).
    for (step, slots, remainder) in [(90 * MS, 2, 233_333), (700 * MS, 21, 0)] {
        let (clk, mut wall) = settled();
        clk.step_utc(step);
        let (before, after) = tick_once(&mut wall, &clk);
        assert_eq!(
            wall.frames_since_resample(),
            0,
            "{step}: the follow re-anchored, so the resample counter restarts"
        );
        assert_eq!(after - before, remainder, "{step}: only r, in ONE event");
        assert_eq!(
            after,
            clk.truth_100ns() - shift_100ns(slots),
            "{step}: the stepped UTC less the relabel, at once"
        );
        let shift = wall.shift();
        assert_eq!(
            (
                shift.slots,
                shift.last_remainder_100ns,
                shift.last_jump_100ns
            ),
            (slots, remainder, remainder),
            "{step}: N and r"
        );
        let st = wall.anchor_stats();
        assert_eq!(
            (
                st.steps_followed,
                st.last_step_us,
                st.holds_followed,
                st.slewed_us,
                st.max_step_us
            ),
            (1, step / 10, 0, 0, (step / 10) as u64),
            "{step}: one followed step, nothing slewed"
        );
    }
}

#[test]
fn a_minus_90_ms_step_moves_the_timeline_forward_by_its_remainder_and_never_holds() {
    // #224 part 2: −90 ms = −3 whole slots (D(−3) = −100 ms) + r = 10 ms. The
    // timeline jumps 10 ms FORWARD at the follow and runs on at once; before,
    // the wall froze for the whole 90 ms (and so did every paced output).
    let (clk, mut wall) = settled();
    clk.step_utc(-90 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 10 * MS, "r = 10 ms, forward");
    let mut prev = after;
    for ms in 1..=95i64 {
        clk.advance_ns(1_000_000);
        let w = wall.now_100ns();
        assert!(w > prev, "ms {ms}: the timeline stood still or went back");
        assert_eq!(
            w,
            clk.truth_100ns() - shift_100ns(-3),
            "ms {ms}: on the relabelled line"
        );
        prev = w;
    }
    assert_eq!(wall.now_100ns(), before + 105 * MS, "95 ms run + r");
    assert_eq!(wall.shift().slots, -3);
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us,
            st.slewed_us
        ),
        (1, 0, -90_000, 0, 0),
        "one followed step, no hold"
    );
}

#[test]
fn a_wide_probe_is_rejected_and_the_next_boundary_follows_the_step_at_once() {
    let (clk, mut wall) = settled();
    clk.step_utc(90 * MS);
    // The probe of this boundary is preempted 400 µs between its monotonic
    // and its UTC read: a wide bracket, rejected by width.
    clk.delay_next_reads(&[400_000]);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "a wide probe moves nothing");
    assert_eq!(wall.anchor_stats().steps_followed, 0);
    // Nothing was armed: the very next boundary follows the step.
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 233_333, "followed at the next boundary: r");
    assert_eq!(after, clk.truth_100ns() - shift_100ns(2));
    let st = wall.anchor_stats();
    assert_eq!((st.steps_followed, st.last_step_us), (1, 90_000));
    assert_eq!(st.wide_brackets, 0, "a probe read is not an anchor sample");
}

#[test]
fn a_wide_confirmation_is_rejected_and_the_next_boundary_follows_the_step_at_once() {
    let (clk, mut wall) = settled();
    clk.step_utc(90 * MS);
    // A clean probe, then every attempt of its confirming sample preempted.
    let mut script = vec![0];
    script.extend([400_000; 8]);
    clk.delay_next_reads(&script);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "an unconfirmed probe moves nothing");
    assert_eq!(
        wall.anchor_stats().wide_brackets,
        1,
        "the wide confirmation"
    );
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 233_333, "followed at the next boundary: r");
    assert_eq!(wall.anchor_stats().steps_followed, 1);
    assert_eq!(clk.reads(), 1 + 10 + (1 + 8) + (1 + 1));
}

#[test]
fn a_lone_narrow_realtime_outlier_is_not_confirmed_and_never_moves_the_wall() {
    let (clk, mut wall) = settled();
    // ONE realtime read 90 ms off; the confirming sample reads the truth.
    clk.outlier_next_reads(&[90 * MS]);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "not confirmed: nothing moves");
    // Through the next resample the wall stays exactly on the true line.
    for frame in 12..=100u64 {
        let (_, after) = tick_once(&mut wall, &clk);
        assert_eq!(after, clk.truth_100ns(), "frame {frame}");
    }
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resampled");
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats::default(),
        "no step, no slew, no wide bracket"
    );
}

#[test]
fn a_step_the_resample_sees_first_is_followed_whole_in_the_same_tick() {
    // The step lands right before the 100th tick: the resample applies its
    // bounded 1 ms and arms; the probe of the same tick confirms the rest and
    // follows it. The followed step counts the armed 1 ms.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(50 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick");
    // +50 ms = 1 slot + r = 16.67 ms: over the tick the timeline moves r —
    // the resample's armed 1 ms, then 15.67 ms at the regrid.
    assert_eq!(after - before, 166_666, "r of the whole step in this tick");
    assert_eq!(after, clk.truth_100ns() - shift_100ns(1));
    let shift = wall.shift();
    assert_eq!(
        (
            shift.slots,
            shift.last_remainder_100ns,
            shift.last_jump_100ns
        ),
        (1, 166_666, 156_666),
        "the regrid's own jump is r less the armed 1 ms"
    );
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.last_step_us,
            st.slewed_us,
            st.max_step_us,
            st.holds_followed
        ),
        (1, 50_000, 1_000, 50_000, 0),
        "the whole 50 ms, of which the resample's 1 ms was slewed"
    );
}

#[test]
fn a_backward_step_the_resample_sees_first_is_relabelled_whole_in_the_same_tick() {
    // −50 ms = −2 slots (D(−2) = −66.67 ms) + r = 16.67 ms. The resample's
    // armed 1 ms hold starts, the probe of the same tick relabels the whole
    // step: the timeline jumps r forward from where the hold froze it.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(-50 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 166_666, "r forward, never a hold");
    clk.advance_ns(49_000_000);
    assert_eq!(wall.now_100ns(), before + 166_666 + 49 * MS, "running");
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), clk.truth_100ns() - shift_100ns(-2));
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us
        ),
        (1, 0, -50_000, 0)
    );
    assert_eq!(wall.shift().slots, -2);
}

#[test]
fn the_probes_after_a_relabelled_backward_step_see_no_new_step() {
    // The UTC anchor took the whole −90 ms: the probes of the next boundaries
    // (e.g. a submit consumer that ticks per job) measure the labels' line,
    // not the relabelled timeline, so they read 0.
    let (clk, mut wall) = settled();
    clk.step_utc(-90 * MS);
    tick_once(&mut wall, &clk);
    ticks(&mut wall, &clk, 3);
    assert_eq!(wall.now_100ns(), clk.truth_100ns() - shift_100ns(-3));
    let st = wall.anchor_stats();
    assert_eq!(
        (st.steps_followed, st.holds_followed, st.last_hold_us),
        (1, 0, 0),
        "one follow, never read again as a new step"
    );
    assert_eq!(wall.probe_stats(), StepProbeStats::default());
}

#[test]
fn a_second_step_after_a_resample_armed_follow_reports_its_own_size() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(50 * MS);
    tick_once(&mut wall, &clk);
    assert_eq!(wall.anchor_stats().last_step_us, 50_000);
    // A second step, as large as the first one's rest: its own 49 ms, not
    // the first step's arming 1 ms again. It is a second epoch: 1 more slot
    // (K = 2), r = 49 ms − (D(2) − D(1)) = 15.6667 ms.
    clk.step_utc(49 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 156_667);
    let st = wall.anchor_stats();
    assert_eq!((st.steps_followed, st.last_step_us), (2, 49_000));
    assert_eq!((wall.shift().slots, wall.shift().epochs), (2, 2));
    assert_eq!(wall.fleet().slots(), 2);
}

#[test]
fn a_2_ms_step_is_slewed_by_the_resample_and_one_just_over_it_is_followed_at_once() {
    // Exactly 2 ms: within the probe's threshold, so the resample slews it —
    // 1 ms, then the 1 ms left, never a followed step.
    let (clk, mut wall) = settled();
    clk.step_utc(2 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "2 ms is not a step for the probe");
    for _ in 0..2 {
        let to_resample = 99 - wall.frames_since_resample();
        ticks(&mut wall, &clk, to_resample);
        let (before, after) = tick_once(&mut wall, &clk);
        assert_eq!(after - before, MS, "one bounded 1 ms per resample");
    }
    assert_eq!(wall.now_100ns(), clk.truth_100ns());
    let st = wall.anchor_stats();
    assert_eq!((st.steps_followed, st.slewed_us), (0, 1_000));
    // 100 ns over 2 ms: a step, followed at once.
    let (clk, mut wall) = settled();
    clk.step_utc(2 * MS + 1);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 2 * MS + 1);
    let st = wall.anchor_stats();
    assert_eq!((st.steps_followed, st.slewed_us), (1, 0));
}

#[test]
fn a_follow_on_the_99th_tick_restarts_the_resample_count() {
    // Review round 1: a follow on the 99th tick used to leave the resample
    // due on the very next tick, and a wall that ticked INSIDE its own hold
    // (the submit consumer ticks per job) then resampled there: it read the
    // rest of the hold as a new −57 ms step, cut the hold to 1 ms, re-armed
    // and "followed" it a second time. A follow is a fresh anchor, so it
    // restarts the resample count (#224 part 2: no hold any more, but a
    // ≤ 1 ms residue hold still can be, and the anchor is fresh either way).
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 98);
    clk.step_utc(-90 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 10 * MS, "the 99th tick relabels: r forward");
    assert_eq!(wall.frames_since_resample(), 0, "a fresh anchor");
    let (_, next) = tick_once(&mut wall, &clk);
    assert_eq!(next, before + 10 * MS + 333_333, "one frame later");
    assert_eq!(wall.frames_since_resample(), 1, "no resample there");
    ticks(&mut wall, &clk, 2);
    assert_eq!(wall.now_100ns(), clk.truth_100ns() - shift_100ns(-3));
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 90_000,
            wide_brackets: 0,
            slewed_us: 0,
            steps_followed: 1,
            last_step_us: -90_000,
            holds_followed: 0,
            last_hold_us: 0,
        },
        "one follow, no hold, nothing slewed"
    );
}

#[test]
fn a_2_5_ms_step_the_resample_sees_first_is_followed_whole_in_the_same_tick() {
    // Review round 1: the resample applies its bounded 1 ms first, so the
    // probe right after it reads only the 1.5 ms left — under 2 ms. The armed
    // 1 ms counts toward the threshold: the whole step is 2.5 ms, a step.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(25_000);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 25_000, "the whole 2.5 ms in this tick");
    assert_eq!(after, clk.truth_100ns());
    let st = wall.anchor_stats();
    assert_eq!(
        (st.steps_followed, st.last_step_us, st.slewed_us),
        (1, 2_500, 1_000)
    );
    // Exactly 2 ms at the resample tick stays a slew: 1 ms now, the rest at
    // the next resample.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(2 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, MS, "the resample's bounded 1 ms only");
    assert_eq!(wall.anchor_stats().steps_followed, 0);
}

#[test]
fn a_settable_wall_never_reads_a_set_as_a_utc_step() {
    // Review round 2: `WallClock::settable` paired its set value with the
    // REAL `Instant::now()`, so the step probe read every set that outran real
    // time as a UTC step and followed it, restarting the resample count
    // (`the_sender_thread_ticks_its_wall_once_per_boundary_passed` counted 2
    // ticks for 3 boundaries). A set moves the settable wall's line with it.
    let (mut wall, clock) = WallClock::settable(0);
    clock.set(1_000_000); // +100 ms at once, far faster than real time
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 1, "a plain tick, no follow");
    clock.advance(-50_000); // a set back is no step either
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 2);
    assert_eq!(wall.anchor_stats(), WallAnchorStats::default());
    assert_eq!(wall.probe_stats(), StepProbeStats::default());
    assert_eq!(wall.now_100ns(), 950_000, "the reads return the set value");
}
