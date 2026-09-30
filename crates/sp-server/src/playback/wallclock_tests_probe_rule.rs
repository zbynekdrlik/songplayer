//! #224: the pure step-probe rule `decide_step_probe`, the anchor's line, and
//! the probe's own telemetry (`StepProbeStats`). Exact values; every
//! comparison of the rule is pinned at its boundary (2 ms detect, the 200 µs
//! wide bracket, the ±1 ms confirmation, the armed 1 ms of a resample).

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// 1 ms in 100-ns units.
const MS: i64 = 10_000;
/// What the test anchor's wall reads at `base`.
const A: i64 = 1_000_000_000;
/// One 30-fps frame of virtual monotonic time (a multiple of 100 ns).
const FRAME_NS: u64 = 33_333_300;
/// 10 ms after `base`, where every rule test probes (ns).
const AT: u64 = 10_000_000;

fn ns(v: u64) -> Duration {
    Duration::from_nanos(v)
}

/// The anchor every rule test measures against: the wall reads `A` at `base`
/// and runs from there.
fn anchor(base: Instant) -> Anchor {
    Anchor {
        instant: base,
        utc_100ns: A,
    }
}

/// That anchor's line `at_ns` after `base` (the production truncation).
fn line(at_ns: u64) -> i64 {
    A + (at_ns / 100) as i64
}

/// A probe read opening `at_ns` after `base`, `width_ns` wide, whose UTC half
/// reads `utc_100ns`.
fn read(base: Instant, at_ns: u64, utc_100ns: i64, width_ns: u64) -> BracketedRead {
    BracketedRead {
        m1: base + ns(at_ns),
        utc_100ns,
        m2: base + ns(at_ns + width_ns),
    }
}

/// A confirming anchor sample paired at `at_ns` after `base`.
fn sample(base: Instant, at_ns: u64, utc_100ns: i64, bracket_ns: u64) -> AnchorSample {
    AnchorSample {
        instant: base + ns(at_ns),
        utc_100ns,
        bracket: ns(bracket_ns),
    }
}

/// A narrow probe at `AT` reading `delta` off the line, confirmed by a sample
/// 1 µs later reading `confirm_delta` off it with `bracket_ns`.
fn probe_then_confirm(
    pending: Option<PendingStep>,
    delta: i64,
    confirm_delta: i64,
    bracket_ns: u64,
) -> ProbeDecision {
    let b = Instant::now();
    let probe = read(b, AT, line(AT) + delta, 0);
    let s = sample(b, AT + 1_000, line(AT + 1_000) + confirm_delta, bracket_ns);
    decide_step_probe(anchor(b), pending, probe, || s)
}

fn follow_of(d: ProbeDecision) -> ProbeFollow {
    match d {
        ProbeDecision::Follow(f) => f,
        other => panic!("expected a follow, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------

#[test]
fn within_2_ms_either_way_the_probe_is_quiet_and_takes_no_confirmation() {
    assert_eq!(STEP_DETECT_100NS, 2 * MS);
    let b = Instant::now();
    for delta in [0, 3_133, 2 * MS, -2 * MS] {
        for width in [0, 400_000] {
            let probe = read(b, AT, line(AT + width / 2) + delta, width);
            let d = decide_step_probe(anchor(b), None, probe, || {
                panic!("a quiet probe takes no confirmation")
            });
            assert_eq!(d, ProbeDecision::Quiet, "delta {delta}, width {width} ns");
        }
    }
}

#[test]
fn just_over_2_ms_either_way_a_narrow_probe_takes_one_confirmation_and_follows() {
    for delta in [2 * MS + 1, -2 * MS - 1] {
        let b = Instant::now();
        let probe = read(b, AT, line(AT) + delta, 0);
        let s = sample(b, AT, line(AT) + delta, 0);
        let calls = Cell::new(0);
        let d = decide_step_probe(anchor(b), None, probe, || {
            calls.set(calls.get() + 1);
            s
        });
        assert_eq!(calls.get(), 1, "delta {delta}: one confirmation");
        let f = follow_of(d);
        assert_eq!(
            f.followed,
            FollowedStep {
                total_100ns: delta,
                direction: StepDirection::of(delta),
            }
        );
        assert_eq!(
            f.step,
            AnchorStep {
                applied_100ns: delta,
                carry_100ns: 0,
            }
        );
    }
}

#[test]
fn a_probe_wider_than_200_us_is_rejected_by_width_and_takes_no_confirmation() {
    let b = Instant::now();
    // 200 µs + 2 ns: its midpoint sits 100 µs + 1 ns in.
    let wide = read(b, AT, line(AT + 100_001) + 90 * MS, 200_002);
    let d = decide_step_probe(anchor(b), None, wide, || {
        panic!("a wide probe takes no confirmation")
    });
    assert_eq!(
        d,
        ProbeDecision::RejectedWide {
            delta_100ns: 90 * MS,
            bracket: ns(200_002),
        }
    );
    // Exactly 200 µs is not wide: it goes on to its confirmation.
    let narrow = read(b, AT, line(AT + 100_000) + 90 * MS, 200_000);
    let s = sample(b, AT + 100_000, line(AT + 100_000) + 90 * MS, 0);
    let f = follow_of(decide_step_probe(anchor(b), None, narrow, || s));
    assert_eq!(f.probe_delta_100ns, 90 * MS);
}

#[test]
fn the_confirmation_must_be_narrow_and_within_1_ms_of_the_probe() {
    let step = 90 * MS;
    for confirm in [step - MS, step, step + MS] {
        let f = follow_of(probe_then_confirm(None, step, confirm, 0));
        assert_eq!(
            f.delta_100ns, confirm,
            "±1 ms of the probe is the same step"
        );
        assert_eq!(f.followed.total_100ns, confirm);
    }
    for confirm in [step - MS - 1, step + MS + 1] {
        let d = probe_then_confirm(None, step, confirm, 0);
        assert!(
            matches!(d, ProbeDecision::Unconfirmed { delta_100ns, confirm_delta_100ns, .. }
                if delta_100ns == step && confirm_delta_100ns == confirm),
            "{confirm}: another step, got {d:?}"
        );
    }
    // A confirmation exactly 200 µs wide is narrow; 201 µs is wide, even when
    // it reads the very same step.
    let f = follow_of(probe_then_confirm(None, step, step, 200_000));
    assert_eq!(f.sample.bracket, ns(200_000));
    let d = probe_then_confirm(None, step, step, 201_000);
    assert!(
        matches!(d, ProbeDecision::Unconfirmed { confirm, .. } if confirm.bracket == ns(201_000)),
        "a wide confirmation, got {d:?}"
    );
}

#[test]
fn a_followed_forward_step_is_applied_whole_at_the_confirming_sample() {
    let b = Instant::now();
    let probe = read(b, AT, line(AT) + 90 * MS, 150);
    let s = sample(b, AT + 50_000, line(AT + 50_000) + 90 * MS + 300, 12_000);
    let f = follow_of(decide_step_probe(anchor(b), None, probe, || s));
    assert_eq!(
        f,
        ProbeFollow {
            sample: s,
            probe_delta_100ns: 90 * MS,
            delta_100ns: 90 * MS + 300,
            wall_100ns: line(AT + 50_000),
            step: AnchorStep {
                applied_100ns: 90 * MS + 300,
                carry_100ns: 0,
            },
            followed: FollowedStep {
                total_100ns: 90 * MS + 300,
                direction: StepDirection::Forward,
            },
        }
    );
    assert_eq!(
        apply_anchor_step(s.instant, f.wall_100ns, f.step.applied_100ns),
        (s.instant, s.utc_100ns),
        "the wall is on the confirming sample's UTC at once"
    );
}

#[test]
fn a_followed_backward_step_is_one_hold_of_the_whole_step() {
    let f = follow_of(probe_then_confirm(None, -90 * MS, -90 * MS, 0));
    assert_eq!(f.step.applied_100ns, -90 * MS);
    assert_eq!(
        f.followed,
        FollowedStep {
            total_100ns: -90 * MS,
            direction: StepDirection::Backward,
        }
    );
    let (instant, utc) = apply_anchor_step(f.sample.instant, f.wall_100ns, f.step.applied_100ns);
    assert_eq!(
        instant,
        f.sample.instant + Duration::from_millis(90),
        "ONE hold"
    );
    assert_eq!(utc, f.wall_100ns, "at the reading the wall shows");
}

#[test]
fn a_step_the_resample_armed_counts_its_applied_part_into_the_total() {
    let armed = |delta: i64, applied: i64| {
        Some(PendingStep {
            delta_100ns: delta,
            applied_100ns: applied,
            direction: StepDirection::of(delta),
        })
    };
    // The resample saw +50 ms and applied 1 ms: the probe follows the 49 ms
    // left, and the step is the whole 50 ms.
    let f = follow_of(probe_then_confirm(armed(50 * MS, MS), 49 * MS, 49 * MS, 0));
    assert_eq!(f.step.applied_100ns, 49 * MS, "the rest is applied");
    assert_eq!(
        f.followed.total_100ns,
        50 * MS,
        "the whole step is reported"
    );
    // Exactly ±1 ms of the armed step still counts it; one tick beyond does not.
    for (rest, total) in [
        (48 * MS, 49 * MS),
        (50 * MS, 51 * MS),
        (48 * MS - 1, 48 * MS - 1),
        (50 * MS + 1, 50 * MS + 1),
    ] {
        let f = follow_of(probe_then_confirm(armed(50 * MS, MS), rest, rest, 0));
        assert_eq!(f.followed.total_100ns, total, "rest {rest}");
    }
    // Backward: a 1 ms hold armed, the rest followed, the whole step reported.
    let f = follow_of(probe_then_confirm(
        armed(-50 * MS, -MS),
        -49 * MS,
        -49 * MS,
        0,
    ));
    assert_eq!(f.followed.total_100ns, -50 * MS);
    assert_eq!(f.followed.direction, StepDirection::Backward);
    // An armed step the other way is another step: nothing counted.
    let f = follow_of(probe_then_confirm(
        armed(50 * MS, MS),
        -49 * MS,
        -49 * MS,
        0,
    ));
    assert_eq!(f.followed.total_100ns, -49 * MS);
}

#[test]
fn inside_a_hold_the_probe_measures_against_the_line_not_the_frozen_wall() {
    // A followed backward step holds the wall at `A` until base + 50 ms; at
    // base + 10 ms the line it runs on reads 40 ms below `A`.
    let b = Instant::now();
    let held = Anchor {
        instant: b + Duration::from_millis(50),
        utc_100ns: A,
    };
    let on_line = read(b, AT, A - 40 * MS, 0);
    let d = decide_step_probe(held, None, on_line, || {
        panic!("the hold in progress is not a new step")
    });
    assert_eq!(d, ProbeDecision::Quiet);
    // A +20 ms step inside the hold is measured against the line, and applied
    // against the frozen wall: it only shortens the hold, never steps back.
    let probe = read(b, AT, A - 20 * MS, 0);
    let s = sample(b, AT, A - 20 * MS, 0);
    let f = follow_of(decide_step_probe(held, None, probe, || s));
    assert_eq!(f.delta_100ns, 20 * MS);
    assert_eq!(f.wall_100ns, A, "the frozen wall");
    assert_eq!(f.step.applied_100ns, -20 * MS);
    assert_eq!(
        f.followed,
        FollowedStep {
            total_100ns: 20 * MS,
            direction: StepDirection::Forward,
        }
    );
    let (instant, utc) = apply_anchor_step(s.instant, f.wall_100ns, f.step.applied_100ns);
    assert_eq!(
        (instant, utc),
        (b + Duration::from_millis(30), A),
        "held until +30 ms instead of +50 ms"
    );
}

#[test]
fn the_anchor_line_extends_back_through_a_hold_while_the_wall_is_frozen_on_it() {
    let b = Instant::now();
    let at5 = b + Duration::from_millis(5);
    let a = Anchor {
        instant: at5,
        utc_100ns: 42,
    };
    assert_eq!(a.line_at(at5), 42);
    assert_eq!(a.line_at(at5 + Duration::from_millis(1)), 42 + MS);
    assert_eq!(a.line_at(at5 + ns(250)), 44, "truncated to 100 ns");
    assert_eq!(a.line_at(at5 - Duration::from_millis(1)), 42 - MS);
    assert_eq!(a.line_at(at5 - ns(250)), 40, "truncated toward the anchor");
    assert_eq!(a.wall_at(at5 - Duration::from_millis(1)), 42, "frozen");
    assert_eq!(a.wall_at(at5 + Duration::from_millis(1)), 42 + MS);
}

// ---------------------------------------------------------------------------
// The probe's telemetry on a WallClock over the VirtualClock
// ---------------------------------------------------------------------------

/// A fresh wall on the true UTC line, ten quiet boundaries in.
fn settled() -> (Arc<VirtualClock>, WallClock) {
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    for _ in 0..10 {
        clk.advance_ns(FRAME_NS);
        wall.tick();
    }
    (clk, wall)
}

fn tick(wall: &mut WallClock, clk: &VirtualClock) {
    clk.advance_ns(FRAME_NS);
    wall.tick();
}

#[test]
fn a_step_followed_at_the_first_probe_reports_no_detect_to_follow_time() {
    let (clk, mut wall) = settled();
    clk.step_utc(90 * MS);
    tick(&mut wall, &clk);
    assert_eq!(wall.anchor_stats().steps_followed, 1);
    assert_eq!(wall.probe_stats(), StepProbeStats::default());
}

#[test]
fn a_rejected_wide_probe_is_counted_and_the_follow_reports_the_time_since_it() {
    let (clk, mut wall) = settled();
    clk.step_utc(90 * MS);
    clk.delay_next_reads(&[400_000]);
    tick(&mut wall, &clk);
    assert_eq!(wall.probe_stats().rejected, 1);
    tick(&mut wall, &clk);
    // One frame, plus the 200 µs the wide probe's midpoint sits before its tick.
    assert_eq!(
        wall.probe_stats(),
        StepProbeStats {
            rejected: 1,
            last_detect_to_follow_us: 33_533,
        }
    );
}

#[test]
fn a_rejected_wide_confirmation_is_counted_and_the_follow_reports_the_time_since_its_probe() {
    let (clk, mut wall) = settled();
    clk.step_utc(90 * MS);
    let mut script = vec![0];
    script.extend([400_000; 8]);
    clk.delay_next_reads(&script);
    tick(&mut wall, &clk);
    tick(&mut wall, &clk);
    assert_eq!(
        wall.probe_stats(),
        StepProbeStats {
            rejected: 1,
            last_detect_to_follow_us: 33_333,
        }
    );
    assert_eq!(wall.anchor_stats().steps_followed, 1);
}

#[test]
fn a_quiet_boundary_after_a_rejected_probe_restarts_the_detection() {
    let (clk, mut wall) = settled();
    // A lone realtime outlier: rejected, then a quiet boundary.
    clk.outlier_next_reads(&[90 * MS]);
    tick(&mut wall, &clk);
    tick(&mut wall, &clk);
    // A real step two boundaries after the outlier is its own detection.
    clk.step_utc(90 * MS);
    tick(&mut wall, &clk);
    assert_eq!(
        wall.probe_stats(),
        StepProbeStats {
            rejected: 1,
            last_detect_to_follow_us: 0,
        }
    );
    assert_eq!(wall.anchor_stats().steps_followed, 1);
}

#[test]
fn a_resample_that_follows_after_every_probe_was_rejected_reports_the_time_since_the_first() {
    // Review round 1: a follow by the resample path (every probe for 200
    // boundaries preempted) reports the detect-to-follow time too, and ends
    // the detection, instead of keeping the previous step's value.
    let clk = VirtualClock::new(0);
    let mut wall = WallClock::new(Box::new(clk.clone()));
    clk.step_utc(90 * MS);
    // Ticks 1–99: wide probes. Tick 100: a clean resample (1 ms + armed),
    // then a wide probe. Ticks 101–199: wide probes. Tick 200: a clean
    // resample confirms the step and follows it; its probe reads clean.
    let mut script = vec![400_000; 99];
    script.push(0);
    script.extend([400_000; 100]);
    script.push(0);
    clk.delay_next_reads(&script);
    for _ in 0..200 {
        tick(&mut wall, &clk);
    }
    // #224 part 2: followed, and relabelled by 2 whole slots.
    assert_eq!(
        wall.now_100ns(),
        clk.truth_100ns() - crate::playback::fleet_shift::shift_100ns(2),
        "followed"
    );
    let st = wall.anchor_stats();
    assert_eq!(
        (st.steps_followed, st.last_step_us, st.slewed_us),
        (1, 90_000, 1_000),
        "the resample path followed it"
    );
    // 199 frames + the 200 µs the first wide probe's midpoint sits back.
    assert_eq!(
        wall.probe_stats(),
        StepProbeStats {
            rejected: 199,
            last_detect_to_follow_us: 6_633_526,
        }
    );
}

#[test]
fn the_part_a_resample_armed_counts_toward_the_2_ms_threshold() {
    let armed = |delta: i64, applied: i64| {
        Some(PendingStep {
            delta_100ns: delta,
            applied_100ns: applied,
            direction: StepDirection::of(delta),
        })
    };
    // A 2.5 ms step: the resample applied 1 ms, the probe reads 1.5 ms. The
    // whole step is over 2 ms: confirmed and followed, the total 2.5 ms.
    let f = follow_of(probe_then_confirm(armed(25_000, MS), 15_000, 15_000, 0));
    assert_eq!(f.step.applied_100ns, 15_000);
    assert_eq!(f.followed.total_100ns, 25_000);
    // Exactly 2 ms in all (1 ms applied + 1 ms read) is not a step.
    let b = Instant::now();
    let probe = read(b, AT, line(AT) + MS, 0);
    let d = decide_step_probe(anchor(b), armed(2 * MS, MS), probe, || {
        panic!("2 ms in all is not a step")
    });
    assert_eq!(d, ProbeDecision::Quiet);
    // An armed step the probe does not see again counts for nothing.
    let probe = read(b, AT, line(AT) + 15_000, 0);
    let d = decide_step_probe(anchor(b), armed(50 * MS, MS), probe, || {
        panic!("1.5 ms alone is not a step")
    });
    assert_eq!(d, ProbeDecision::Quiet);
}
