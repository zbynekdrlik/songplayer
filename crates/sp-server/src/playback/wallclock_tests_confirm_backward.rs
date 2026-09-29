//! #147 (design record 5850063723, Approach 1): a CONFIRMED backward UTC step
//! is followed as ONE hold, never a backward step.
//!
//! dantesync 1.12.0 makes one coordinated fleet date step per night (04:00
//! local) of up to ~±1.5 s, in either direction. Held at 1 ms per ~3.3 s
//! resample, a −1.5 s step took ~83 min to absorb, with SongPlayer's stamps up
//! to 1.5 s ahead of the fleet the whole time.
//!
//! The rule mirrors the forward one (`wallclock_tests_confirm.rs`), with the
//! same ±1 ms tolerance and the same bracket bound:
//! - a narrow resample over 1 ms backward applies a 1 ms hold and ARMS;
//! - the next narrow resample that sees the same step (±1 ms) FOLLOWS the rest
//!   as ONE hold `(instant + |rest|, wall(instant))`: the wall freezes, then
//!   runs on the corrected UTC line;
//! - a lone outlier, a wide bracket or a different step stays the 1 ms hold.
//!
//! Pure rule + a WallClock over the [`VirtualClock`], with exact values. Since
//! #224 the per-boundary step probe follows a real step at the boundary it
//! lands, as ONE hold of the WHOLE step; the resample's own confirm path is
//! driven with realtime outliers scripted on the resample's reads.

use super::*;
use std::time::{Duration, Instant};

/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;
/// 1 ms in 100-ns units.
const MS: i64 = 10_000;
/// A dantesync 1.12.0 nightly date step backward: −1.5 s in 100-ns units.
const STEP: i64 = -15_000_000;

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

/// Run one resample interval whose resampling 100th tick reads with `delays`
/// and `outliers` scripted (the resample's anchor reads come first); the step
/// probe of every tick (#224) and every other read see the true UTC.
fn resample_reading(
    wall: &mut WallClock,
    clk: &VirtualClock,
    delays: &[u64],
    outliers: &[i64],
) -> (i64, i64) {
    for _ in 0..99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    clk.delay_next_reads(delays);
    clk.outlier_next_reads(outliers);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resamples");
    (before, wall.now_100ns())
}

/// A step armed by a resample that measured `delta_100ns` and applied the
/// bounded `applied_100ns`, in `direction`.
fn armed(delta_100ns: i64, applied_100ns: i64, direction: StepDirection) -> Option<PendingStep> {
    Some(PendingStep {
        delta_100ns,
        applied_100ns,
        direction,
    })
}

/// A followed backward step (one hold) of `total_100ns`.
fn backward(total_100ns: i64) -> Option<FollowedStep> {
    Some(FollowedStep {
        total_100ns,
        direction: StepDirection::Backward,
    })
}

// ---------------------------------------------------------------------------
// The pure rule
// ---------------------------------------------------------------------------

#[test]
fn a_clamped_backward_narrow_resample_holds_1_ms_and_arms_the_step() {
    let d = decide_anchor_step(None, STEP, true);
    assert_eq!(
        d.step,
        AnchorStep {
            applied_100ns: -MS,
            carry_100ns: STEP + MS,
        },
        "the usual 1 ms hold"
    );
    assert_eq!(d.followed, None);
    assert_eq!(
        d.pending,
        armed(STEP, -MS, StepDirection::Backward),
        "a narrow backward clamp arms, exactly like a forward one"
    );
}

#[test]
fn the_second_narrow_backward_resample_follows_the_rest_as_one_hold() {
    // The 1 ms hold already applied leaves −1.499 s: the same step, seen twice.
    let d = decide_anchor_step(armed(STEP, -MS, StepDirection::Backward), STEP + MS, true);
    assert_eq!(
        d.step,
        AnchorStep {
            applied_100ns: STEP + MS,
            carry_100ns: 0,
        },
        "the whole rest in ONE event"
    );
    assert!(!d.step.is_clamped(), "a followed hold is not a slew");
    assert_eq!(d.followed, backward(STEP), "the whole step, signed");
    assert_eq!(d.pending, None);
}

#[test]
fn backward_confirmation_holds_within_1_ms_either_way_and_not_one_tick_beyond() {
    // delta + applied₁ = delta − 1 ms must lie within ±1 ms of the armed −1.5 s.
    for delta in [STEP, STEP + 2 * MS] {
        let d = decide_anchor_step(armed(STEP, -MS, StepDirection::Backward), delta, true);
        assert_eq!(
            d.followed,
            backward(delta - MS),
            "delta {delta}: ±1 ms of the armed step is the same step"
        );
        assert_eq!(d.step.applied_100ns, delta, "delta {delta}: one hold");
        assert_eq!(d.pending, None);
    }
    for delta in [STEP - 1, STEP + 2 * MS + 1] {
        let d = decide_anchor_step(armed(STEP, -MS, StepDirection::Backward), delta, true);
        assert_eq!(d.followed, None, "delta {delta}: a different step");
        assert_eq!(
            d.step.applied_100ns, -MS,
            "delta {delta}: still a 1 ms hold"
        );
        assert_eq!(
            d.pending,
            armed(delta, -MS, StepDirection::Backward),
            "delta {delta}: it arms itself instead"
        );
    }
}

#[test]
fn a_wide_backward_bracket_never_arms_or_confirms() {
    let d = decide_anchor_step(None, STEP, false);
    assert_eq!(d.step.applied_100ns, -MS);
    assert_eq!(d.pending, None, "a preempted sample never arms");
    let d = decide_anchor_step(armed(STEP, -MS, StepDirection::Backward), STEP + MS, false);
    assert_eq!(d.followed, None, "a preempted sample never confirms");
    assert_eq!(d.step.applied_100ns, -MS);
    assert_eq!(d.pending, None);
}

#[test]
fn a_step_is_never_confirmed_by_a_read_the_other_way() {
    // An armed forward step and a backward read (and vice versa) are two
    // different steps: the read arms its own direction instead.
    let d = decide_anchor_step(armed(50 * MS, MS, StepDirection::Forward), -49 * MS, true);
    assert_eq!(d.followed, None);
    assert_eq!(d.step.applied_100ns, -MS);
    assert_eq!(d.pending, armed(-49 * MS, -MS, StepDirection::Backward));
    let d = decide_anchor_step(armed(-50 * MS, -MS, StepDirection::Backward), 49 * MS, true);
    assert_eq!(d.followed, None);
    assert_eq!(d.step.applied_100ns, MS);
    assert_eq!(d.pending, armed(49 * MS, MS, StepDirection::Forward));
}

#[test]
fn a_backward_delta_within_1_ms_is_a_plain_hold_and_clears_the_armed_step() {
    // Exactly −1 ms would "match" an armed −2 ms step, but it is not a step.
    let d = decide_anchor_step(armed(-2 * MS, -MS, StepDirection::Backward), -MS, true);
    assert_eq!(d.step.applied_100ns, -MS);
    assert_eq!(d.followed, None);
    assert_eq!(d.pending, None);
}

#[test]
fn a_step_direction_is_the_sign_of_its_delta_and_names_itself_in_the_log() {
    assert_eq!(StepDirection::of(1), StepDirection::Forward);
    assert_eq!(StepDirection::of(50 * MS), StepDirection::Forward);
    assert_eq!(StepDirection::of(-1), StepDirection::Backward);
    assert_eq!(StepDirection::of(STEP), StepDirection::Backward);
    assert_eq!(StepDirection::Forward.as_str(), "forward");
    assert_eq!(StepDirection::Backward.as_str(), "backward");
}

#[test]
fn apply_step_turns_a_whole_followed_backward_step_into_one_hold() {
    let at = Instant::now();
    let (inst, utc) = apply_anchor_step(at, 1_000_000, STEP + MS);
    assert_eq!(inst, at + Duration::from_millis(1_499), "held for |rest|");
    assert_eq!(utc, 1_000_000, "at the wall it already shows");
    let hold_end = at + Duration::from_millis(1_499);
    assert_eq!(wall_at(inst, utc, at), 1_000_000);
    assert_eq!(wall_at(inst, utc, hold_end), 1_000_000, "still frozen");
    assert_eq!(
        wall_at(inst, utc, hold_end + Duration::from_millis(1)),
        1_000_000 + MS,
        "then on the corrected line: old line − 1.499 s"
    );
}

#[test]
fn a_followed_hold_is_recorded_with_its_signed_step_and_its_hold_length() {
    let mut st = WallAnchorStats::default();
    st.record_follow(
        &FollowedStep {
            total_100ns: STEP,
            direction: StepDirection::Backward,
        },
        &AnchorStep {
            applied_100ns: STEP + MS,
            carry_100ns: 0,
        },
    );
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us
        ),
        (1, 1, -1_500_000, 1_499_000)
    );
    // A later forward step counts as a step, not a hold; the last hold stays.
    st.record_follow(
        &FollowedStep {
            total_100ns: 50 * MS,
            direction: StepDirection::Forward,
        },
        &AnchorStep {
            applied_100ns: 49 * MS,
            carry_100ns: 0,
        },
    );
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us
        ),
        (2, 1, 50_000, 1_499_000)
    );
}

// ---------------------------------------------------------------------------
// WallClock over the virtual clock
// ---------------------------------------------------------------------------

#[test]
fn a_minus_1_5_s_step_is_one_hold_from_the_next_boundary_and_the_wall_never_goes_back() {
    // #224: the probe of the first boundary after the step follows it as ONE
    // hold of the whole 1.5 s; no 1 ms arming hold first (#147).
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(STEP);
    clk.advance_ns(FRAME_NS);
    let frozen = wall.now_100ns();
    wall.tick();
    assert_eq!(
        wall.now_100ns(),
        frozen,
        "a hold starts at the reading shown"
    );
    assert_eq!(
        frozen - clk.truth_100ns(),
        -STEP,
        "1.5 s ahead of the new UTC"
    );
    let st = wall.anchor_stats();
    assert_eq!(st.steps_followed, 1, "followed at the first boundary");
    assert_eq!(st.holds_followed, 1, "exactly one hold event");
    // The wall freezes for the whole step, then runs on the UTC line. The
    // boundaries keep ticking through the hold (every ~33 ms, as a submit
    // consumer does): each probe inside it sees no new step.
    let mut prev = frozen;
    for ms in 1..=1_600i64 {
        clk.advance_ns(1_000_000);
        if ms % 33 == 0 {
            wall.tick();
        }
        let w = wall.now_100ns();
        assert!(w >= prev, "ms {ms}: the wall went back");
        assert_eq!(
            w,
            frozen.max(clk.truth_100ns()),
            "ms {ms}: frozen until the UTC line reaches it, then on it"
        );
        prev = w;
    }
    assert_eq!(wall.now_100ns(), clk.truth_100ns(), "on the UTC line");
    // The next resample is normal again (48 ticks since the follow restarted
    // the count).
    while wall.frames_since_resample() < 99 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    clk.advance_ns(FRAME_NS);
    let before = wall.now_100ns();
    wall.tick();
    assert_eq!(wall.frames_since_resample(), 0, "the 100th tick resampled");
    assert_eq!(wall.now_100ns(), before);
    assert_eq!(wall.now_100ns(), clk.truth_100ns());
    assert_eq!(
        wall.anchor_stats(),
        WallAnchorStats {
            max_step_us: 1_500_000,
            wide_brackets: 0,
            slewed_us: 0,
            steps_followed: 1,
            last_step_us: -1_500_000,
            holds_followed: 1,
            last_hold_us: 1_500_000,
        }
    );
}

#[test]
fn a_lone_minus_1_5_s_outlier_then_a_normal_read_holds_the_wall_at_most_1_ms() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    // One narrow resample read sees UTC 1.5 s behind; every other read (the
    // step probe right after it included, #224) reads the true line.
    let (before, after) = resample_reading(&mut wall, &clk, &[], &[STEP]);
    assert_eq!(
        after, before,
        "the outlier starts a hold, never a step back"
    );
    clk.advance_ns(500_000);
    assert_eq!(wall.now_100ns(), before, "still holding half-way");
    clk.advance_ns(500_000);
    assert_eq!(wall.now_100ns(), before, "held for exactly 1 ms");
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), before + MS, "then running again");
    let (before, after) = resample(&mut wall, &clk);
    assert_eq!(
        after - before,
        MS,
        "the normal read takes the 1 ms back out"
    );
    assert_eq!(after, clk.truth_100ns(), "back on the true line");
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.steps_followed,
            st.holds_followed,
            st.last_step_us,
            st.last_hold_us,
            st.slewed_us
        ),
        (0, 0, 0, 0, 1_000),
        "an outlier is never followed"
    );
}

#[test]
fn a_wide_backward_resample_never_arms_so_the_next_narrow_read_only_arms() {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    // Resample 1: every attempt preempted by 400 µs AND reading UTC 1.5 s
    // behind — a wide bracket. The 200 µs midpoint pairing error reads it as
    // −1.4998 s. (A hold at a wide bracket's midpoint reads below the
    // harness's earlier read — the documented harness artifact — so only the
    // stats are asserted here.)
    resample_reading(&mut wall, &clk, &[400_000; 8], &[STEP; 8]);
    // Resample 2 is narrow and reads the same −1.5 s once. Had resample 1
    // armed, it would follow the rest as a 1.499 s hold; it only arms.
    let (before, after) = resample_reading(&mut wall, &clk, &[], &[STEP]);
    assert_eq!(after, before, "a 1 ms hold, never a step back");
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), before, "held 1 ms");
    clk.advance_ns(1_000_000);
    assert_eq!(wall.now_100ns(), before + MS, "then running: no long hold");
    let st = wall.anchor_stats();
    assert_eq!(
        (
            st.wide_brackets,
            st.steps_followed,
            st.holds_followed,
            st.slewed_us,
            st.max_step_us
        ),
        (1, 0, 0, 2_000, 1_499_800),
        "a wide read never arms"
    );
}
