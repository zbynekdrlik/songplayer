//! #147 (design record 5845527884, Approach 1 (b)): the WallClock follows a
//! CONFIRMED forward UTC step (a dantesync fleet date step, ~+50 ms about
//! every 47 min) in ONE re-anchor, like every camera-box sender follows
//! `CLOCK_REALTIME` at once. A lone outlier stays bounded at 1 ms and a
//! backward correction is still held. Pure rule + a WallClock over the
//! [`VirtualClock`]; exact values, so every comparison in the rule is pinned.

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

fn pending(delta_100ns: i64, applied_100ns: i64) -> Option<PendingStep> {
    Some(PendingStep {
        delta_100ns,
        applied_100ns,
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
    assert_eq!(d.followed_100ns, None);
    assert_eq!(d.pending, pending(50 * MS, MS));
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
    assert_eq!(d.followed_100ns, Some(50 * MS), "the whole step");
    assert_eq!(d.pending, None);
}

#[test]
fn confirmation_holds_within_1_ms_either_way_and_not_one_tick_beyond() {
    // 49 ms left + 1 ms applied = 50 ms: exactly on the armed step.
    for delta in [48 * MS, 50 * MS] {
        let d = decide_anchor_step(pending(50 * MS, MS), delta, true);
        assert_eq!(
            d.followed_100ns,
            Some(delta + MS),
            "delta {delta}: ±1 ms of the armed step is the same step"
        );
        assert_eq!(d.step.applied_100ns, delta);
    }
    for delta in [48 * MS - 1, 50 * MS + 1] {
        let d = decide_anchor_step(pending(50 * MS, MS), delta, true);
        assert_eq!(d.followed_100ns, None, "delta {delta}: a different step");
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
    assert_eq!(d.followed_100ns, None, "a preempted sample never confirms");
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.pending, None);
}

#[test]
fn a_delta_within_1_ms_is_applied_as_is_and_clears_the_armed_step() {
    // Exactly 1 ms would "match" an armed 2 ms step, but it is not a step:
    // the plain rule applies it and nothing is followed or armed.
    let d = decide_anchor_step(pending(2 * MS, MS), MS, true);
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.followed_100ns, None);
    assert_eq!(d.pending, None);
    let d = decide_anchor_step(None, 3_133, true);
    assert_eq!(d.step.applied_100ns, 3_133);
    assert_eq!(d.pending, None, "an unclamped resample arms nothing");
}

#[test]
fn a_confirmed_backward_step_is_still_held_never_followed() {
    let d = decide_anchor_step(None, -50 * MS, true);
    assert_eq!(d.step.applied_100ns, -MS, "a ≤ 1 ms hold");
    assert_eq!(d.pending, None, "backward never arms");
    // Even an armed backward step (never produced by the rule) is not followed.
    let d = decide_anchor_step(pending(-50 * MS, -MS), -49 * MS, true);
    assert_eq!(d.followed_100ns, None);
    assert_eq!(d.step.applied_100ns, -MS);
    assert_eq!(d.pending, None);
}

#[test]
fn a_followed_step_is_recorded_as_its_total() {
    let mut st = WallAnchorStats::default();
    st.record_follow(50 * MS + 300);
    st.record_follow(50_277 * 10);
    assert_eq!(st.steps_followed, 2);
    assert_eq!(st.last_step_us, 50_277, "the last step's total, in µs");
}

// ---------------------------------------------------------------------------
// WallClock over the virtual clock
// ---------------------------------------------------------------------------

#[test]
fn a_lone_plus_50_ms_outlier_then_a_normal_read_moves_the_wall_at_most_1_ms() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    // One narrow sample reads UTC 50 ms ahead, the next reads the true line.
    clk.step_utc(50 * MS);
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after - before, MS, "the outlier moves the wall 1 ms only");
    clk.step_utc(-50 * MS);
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
fn a_confirmed_minus_50_ms_step_is_still_held_at_1_ms_per_resample() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(-50 * MS);
    for r in 1..=3i64 {
        let (before, after) = resample(&mut wall, &clk);
        assert_eq!(after, before, "resample {r}: a hold, never a step back");
        clk.advance_ns(1_000_000);
        assert_eq!(
            wall.now_100ns() - clk.truth_100ns(),
            (50 - r) * MS,
            "resample {r}: 1 ms closer to truth, no more"
        );
    }
    assert_eq!(wall.anchor_stats().steps_followed, 0);
}

#[test]
fn a_step_preempted_on_its_confirming_resample_slews_on_until_two_clean_reads_agree() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(50 * MS);
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after - before, MS);
    // Every attempt of the confirming resample is preempted by 400 µs: wide.
    clk.delay_next_reads(&[400_000; 8]);
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after - before, MS, "a wide resample only slews");
    // Two clean resamples in a row: the first re-arms, the second follows.
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(after - before, MS);
    let (_, after) = resample(&mut wall, &clk);
    assert_eq!(after, clk.truth_100ns(), "followed at the 4th resample");
    let st = wall.anchor_stats();
    assert_eq!(st.steps_followed, 1);
    assert_eq!(st.wide_brackets, 1);
}
