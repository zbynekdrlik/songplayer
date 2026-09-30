//! #147 (design record 5845527884, Approach 1 (b)): the WallClock follows a
//! CONFIRMED forward UTC step (a dantesync fleet date step, ~+50 ms about
//! every 47 min) in ONE re-anchor, like every camera-box sender follows
//! `CLOCK_REALTIME` at once. A lone outlier stays bounded at 1 ms. Pure rule +
//! a WallClock over the [`VirtualClock`]; exact values, so every comparison in
//! the rule is pinned. The backward direction (design record 5850063723, ONE
//! hold) is in `wallclock_tests_confirm_backward.rs`. Since #224 the
//! per-boundary step probe follows a real step at the boundary it lands
//! (`wallclock_tests_probe.rs`), so the resample's own confirm path is driven
//! here with a realtime outlier scripted on the resample's read.

use super::*;

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;
/// 1 ms in 100-ns units.
const MS: i64 = 10_000;

/// Run 100 frames (one resample interval) and return the wall reading just
/// before and just after the resampling 100th tick.
fn resample(wall: &mut WallClock, clk: &VirtualClock) -> (i64, i64) {
    for _ in 0..99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resamples");
    (before, wall.now_100ns())
}

/// Run one resample interval whose resampling 100th tick reads `outliers`
/// first (the resample's anchor read); the step probe that follows it in the
/// same tick (#224) and every other read see the true UTC.
fn resample_reading(wall: &mut WallClock, clk: &VirtualClock, outliers: &[i64]) -> (i64, i64) {
    for _ in 0..99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    clk.outlier_next_reads(outliers);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resamples");
    (before, wall.now_100ns())
}

/// Advance one frame and tick; the wall just before and just after the tick.
fn tick_once(wall: &mut WallClock, clk: &VirtualClock) -> (i64, i64) {
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    (before, wall.now_100ns())
}

/// An armed step; its direction is the sign of `delta_100ns`.
fn pending(delta_100ns: i64, applied_100ns: i64) -> Option<PendingStep> {
    let direction = if delta_100ns > 0 {
        StepDirection::Forward
    } else {
        StepDirection::Backward
    };
    Some(PendingStep {
        delta_100ns,
        applied_100ns,
        direction,
    })
}

/// A followed forward step of `total_100ns`.
fn forward(total_100ns: i64) -> Option<FollowedStep> {
    Some(FollowedStep {
        total_100ns,
        direction: StepDirection::Forward,
    })
}

// ---------------------------------------------------------------------------
// The pure rule
// ---------------------------------------------------------------------------

#[test]
fn a_clamped_forward_narrow_resample_applies_1_ms_and_arms_the_step() {
    let d = decide_anchor_step(None, 50 * MS, true);
    assert_eq!(d.step, bounded_anchor_update(50 * MS));
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.followed, None);
    assert_eq!(d.pending, pending(50 * MS, MS));
    assert_eq!(d.pending.unwrap().direction, StepDirection::Forward);
}

#[test]
fn the_second_narrow_resample_measuring_the_same_step_follows_the_rest_at_once() {
    let d = decide_anchor_step(pending(50 * MS, MS), 49 * MS, true);
    assert_eq!(
        d.step,
        AnchorStep {
            applied_100ns: 49 * MS,
            carry_100ns: 0,
        }
    );
    assert!(!d.step.is_clamped(), "a followed step is not a slew");
    assert_eq!(d.followed, forward(50 * MS), "the whole step");
    assert_eq!(d.pending, None);
}

#[test]
fn confirmation_holds_within_1_ms_either_way_and_not_one_tick_beyond() {
    // 49 ms left + 1 ms applied = 50 ms: exactly on the armed step.
    for delta in [48 * MS, 50 * MS] {
        let d = decide_anchor_step(pending(50 * MS, MS), delta, true);
        assert_eq!(
            d.followed,
            forward(delta + MS),
            "delta {delta}: ±1 ms of the armed step is the same step"
        );
        assert_eq!(d.step.applied_100ns, delta);
    }
    for delta in [48 * MS - 1, 50 * MS + 1] {
        let d = decide_anchor_step(pending(50 * MS, MS), delta, true);
        assert_eq!(d.followed, None, "delta {delta}: a different step");
        assert_eq!(d.step.applied_100ns, MS, "delta {delta}: still bounded");
        assert_eq!(
            d.pending,
            pending(delta, MS),
            "delta {delta}: it arms itself instead"
        );
    }
}

#[test]
fn a_wide_bracket_never_arms_or_confirms() {
    let d = decide_anchor_step(None, 50 * MS, false);
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.pending, None, "a preempted sample never arms");
    let d = decide_anchor_step(pending(50 * MS, MS), 49 * MS, false);
    assert_eq!(d.followed, None, "a preempted sample never confirms");
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.pending, None);
}

#[test]
fn a_delta_within_1_ms_is_applied_as_is_and_clears_the_armed_step() {
    // Exactly 1 ms would "match" an armed 2 ms step, but it is not a step:
    // the plain rule applies it and nothing is followed or armed.
    let d = decide_anchor_step(pending(2 * MS, MS), MS, true);
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.followed, None);
    assert_eq!(d.pending, None);
    let d = decide_anchor_step(None, 3_133, true);
    assert_eq!(d.step.applied_100ns, 3_133);
    assert_eq!(d.pending, None, "an unclamped resample arms nothing");
}

#[test]
fn a_followed_forward_step_is_recorded_as_its_signed_total_and_is_no_hold() {
    let mut st = WallAnchorStats::default();
    let rest = AnchorStep {
        applied_100ns: 49 * MS + 300,
        carry_100ns: 0,
    };
    st.record_follow(
        &FollowedStep {
            total_100ns: 50 * MS + 300,
            direction: StepDirection::Forward,
        },
        &rest,
    );
    st.record_follow(
        &FollowedStep {
            total_100ns: 50_277 * 10,
            direction: StepDirection::Forward,
        },
        &rest,
    );
    assert_eq!(st.steps_followed, 2);
    assert_eq!(st.last_step_us, 50_277, "the last step's total, in µs");
    assert_eq!(st.holds_followed, 0, "a forward step is never a hold");
    assert_eq!(st.last_hold_us, 0);
}

// ---------------------------------------------------------------------------
// WallClock over the virtual clock
// ---------------------------------------------------------------------------

#[test]
fn a_lone_plus_50_ms_outlier_then_a_normal_read_moves_the_wall_at_most_1_ms() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    // One narrow resample read sees UTC 50 ms ahead; every other read (the
    // step probe right after it included, #224) reads the true line.
    let (before, after) = resample_reading(&mut wall, &clk, &[50 * MS]);
    assert_eq!(after - before, MS, "the outlier moves the wall 1 ms only");
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after, before, "the normal read holds the 1 ms back out");
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), clk.truth_100ns(), "back on the true line");
    let st = wall.anchor_stats();
    assert_eq!(st.steps_followed, 0, "an outlier is never followed");
    assert_eq!(st.last_step_us, 0);
    assert_eq!(st.slewed_us, 1_000);
}

#[test]
fn a_step_whose_first_probe_is_preempted_is_followed_at_the_next_boundary() {
    // #224: the first read of a +50 ms step is preempted (a wide probe) and
    // rejected by width. Nothing is armed, so the next boundary's clean probe
    // follows the step in one event — no longer slewed at 1 ms per resample
    // until two clean resamples agree (#147).
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(50 * MS);
    clk.delay_next_reads(&[400_000]);
    let (before, after) = tick_once(&mut wall, &clk);
    assert_eq!(after, before, "a wide probe moves nothing");
    let (before, after) = tick_once(&mut wall, &clk);
    // #224 part 2: +50 ms = 1 slot relabelled + r = 16.67 ms on the timeline.
    assert_eq!(after - before, 166_666, "followed at the next boundary: r");
    assert_eq!(
        after,
        clk.truth_100ns() - crate::playback::fleet_shift::shift_100ns(1)
    );
    let st = wall.anchor_stats();
    assert_eq!(st.steps_followed, 1);
    assert_eq!(st.last_step_us, 50_000);
    assert_eq!(st.wide_brackets, 0, "a probe read is not an anchor sample");
    assert_eq!(st.holds_followed, 0);
    assert_eq!(st.slewed_us, 0, "nothing slewed");
}
