//! #224 (design record 5890605448, Approach 1): the WallClock follows a
//! dantesync date step at the boundary it lands, not two 100-frame resamples
//! (3.3–6.7 s) later.
//!
//! Every tick takes ONE bracketed read (the step probe) and measures it
//! against the line the wall runs on. Over 2 ms from a narrow bracket, a full
//! anchor sample is taken in the same tick; when both agree within 1 ms the
//! step is followed in ONE event, a step ahead when forward and ONE hold when
//! backward. A wide probe, or a confirmation that is wide or reads another
//! step, is rejected, and nothing is armed, so the next boundary still follows
//! a real step at once. The 100-frame resample keeps slewing everything
//! within 2 ms.
//!
//! A WallClock over the [`VirtualClock`], exact values. The pure rule
//! (`decide_step_probe`) is pinned in `wallclock_tests_probe_rule.rs`.

use std::sync::Arc;

use super::*;

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
fn a_plus_90_ms_and_a_plus_700_ms_step_are_followed_whole_at_the_boundary_they_land() {
    // The 02:00Z nightly step (89.7 ms) and a full-day step (~0.7 s).
    for step in [90 * MS, 700 * MS] {
        let (clk, mut wall) = settled();
        clk.step_utc(step);
        let (before, after) = tick_once(&mut wall, &clk);
        assert_eq!(
            wall.frames_since_resample(),
            0,
            "{step}: the follow re-anchored, so the resample counter restarts"
        );
        assert_eq!(after - before, step, "{step}: the whole step in ONE event");
        assert_eq!(
            after,
            clk.truth_100ns(),
            "{step}: on the stepped UTC at once"
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
fn a_minus_90_ms_step_is_one_hold_at_the_boundary_it_lands_and_the_wall_never_goes_back() {
    let (clk, mut wall) = settled();
    clk.step_utc(-90 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "a hold starts at the reading the wall shows");
    let mut prev = after;
    for ms in 1..=95i64 {
        clk.advance_ns(1_000_000);
        let w = wall.now_100ns();
        assert!(w >= prev, "ms {ms}: the wall went back");
        assert_eq!(
            w,
            before.max(clk.truth_100ns()),
            "ms {ms}: frozen until the UTC line reaches it, then on it"
        );
        prev = w;
    }
    assert_eq!(wall.now_100ns(), before + 5 * MS, "held exactly 90 ms");
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us,
            st.slewed_us
        ),
        (1, 1, -90_000, 90_000, 0),
        "ONE hold of the whole step"
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
    assert_eq!(after - before, 90 * MS, "followed at the next boundary");
    assert_eq!(after, clk.truth_100ns());
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
    assert_eq!(after - before, 90 * MS, "followed at the next boundary");
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
    assert_eq!(after - before, 50 * MS, "the whole step in this tick");
    assert_eq!(after, clk.truth_100ns());
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.last_step_us,
            st.slewed_us,
            st.max_step_us
        ),
        (1, 50_000, 1_000, 50_000),
        "the whole 50 ms, of which the resample's 1 ms was slewed"
    );
}

#[test]
fn a_backward_step_the_resample_sees_first_is_one_hold_of_the_whole_step_in_the_same_tick() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 99);
    clk.step_utc(-50 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "a hold, never a step back");
    clk.advance_ns(49_000_000);
    assert_eq!(wall.now_100ns(), before, "still frozen at 49 ms");
    clk.advance_ns(1_000_000);
    assert_eq!(
        wall.now_100ns(),
        before,
        "frozen 50 ms: the resample's 1 ms is inside it"
    );
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), before + MS, "then on the UTC line");
    assert_eq!(wall.now_100ns(), clk.truth_100ns());
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us
        ),
        (1, 1, -50_000, 50_000)
    );
}

#[test]
fn a_probe_inside_a_followed_hold_sees_no_new_step() {
    let (clk, mut wall) = settled();
    clk.step_utc(-90 * MS);
    let (before, _) = tick_once(&mut wall, &clk);
    // Two more boundaries tick INSIDE the 90 ms hold (e.g. a submit consumer
    // that ticks per job), then one after it.
    ticks(&mut wall, &clk, 2);
    assert_eq!(wall.now_100ns(), before, "still the one hold");
    ticks(&mut wall, &clk, 1);
    assert_eq!(
        wall.now_100ns(),
        clk.truth_100ns(),
        "on the UTC line after it"
    );
    let st = wall.anchor_stats();
    assert_eq!(
        (st.steps_followed, st.holds_followed, st.last_hold_us),
        (1, 1, 90_000),
        "a hold in progress is never read as a new step"
    );
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
    // the first step's arming 1 ms again.
    clk.step_utc(49 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after - before, 49 * MS);
    let st = wall.anchor_stats();
    assert_eq!((st.steps_followed, st.last_step_us), (2, 49_000));
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
fn a_resample_never_lands_inside_a_hold_the_probe_just_followed() {
    // Review round 1: a follow on the 99th tick used to leave the resample
    // due on the very next tick, and a wall that ticks INSIDE its own hold
    // (the submit consumer ticks per job) then resampled there: it read the
    // rest of the hold as a new −57 ms step, cut the hold to 1 ms, re-armed
    // and "followed" it a second time. A follow is a fresh anchor, so it
    // restarts the resample count.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    ticks(&mut wall, &clk, 98);
    clk.step_utc(-90 * MS);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "the 99th tick follows the step: ONE hold");
    assert_eq!(wall.frames_since_resample(), 0, "a fresh anchor");
    // One boundary later, still inside the 90 ms hold.
    let (_, inside) = tick_once(&mut wall, &clk);
    assert_eq!(inside, before, "still the one hold");
    assert_eq!(wall.frames_since_resample(), 1, "no resample inside it");
    ticks(&mut wall, &clk, 2);
    assert_eq!(wall.now_100ns(), clk.truth_100ns(), "then on the UTC line");
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 90_000,
            wide_brackets: 0,
            slewed_us: 0,
            steps_followed: 1,
            last_step_us: -90_000,
            holds_followed: 1,
            last_hold_us: 90_000,
        },
        "one follow, one hold, nothing slewed"
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
