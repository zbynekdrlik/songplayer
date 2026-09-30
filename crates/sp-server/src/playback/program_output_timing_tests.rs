//! #210: the `SP-program` sender's per-boundary timing window, exact pins.
//! Wired via `#[cfg(test)] #[path = "program_output_timing_tests.rs"] mod tests;`.

use super::*;
use crate::playback::stat_window::Worst;
use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
use sp_core::genlock::{GENLOCK_GRID_FPS, UNITS_PER_SECOND};

/// A grid boundary (100 ns).
const B: i64 = 17_907_771_311_333_333;

/// A boundary taken at once whose VBAN hand-off came `feed_us` after it.
fn fed_late(stamp: i64, feed_us: i64) -> BoundaryMarks {
    let fed = stamp + feed_us * 10;
    BoundaryMarks {
        stamp_100ns: stamp,
        taken_100ns: stamp,
        fed_100ns: fed,
        submit_start_100ns: fed,
        submitted_100ns: fed,
    }
}

/// A boundary whose three figures are `ready_us`, `feed_us` and `submit_us`.
fn figures(ready_us: i64, feed_us: i64, submit_us: i64) -> BoundaryMarks {
    BoundaryMarks {
        stamp_100ns: B,
        taken_100ns: B + ready_us * 10,
        fed_100ns: B + feed_us * 10,
        submit_start_100ns: B + feed_us * 10,
        submitted_100ns: B + (feed_us + submit_us) * 10,
    }
}

#[test]
fn the_limits_are_the_designed_ones() {
    assert_eq!(STAGE_SLOW_US, 5_000, "5 ms");
    assert_eq!(VBAN_FEED_SLOW_US, 10_000, "10 ms");
    assert_eq!(
        VBAN_FEED_BUDGET_US, 66_666,
        "#210 part 2: VBAN's send latency L — a block's first packet is due 66.7 ms after its boundary"
    );
    assert_eq!(
        VBAN_FEED_BUDGET_US as i64,
        VBAN_SEND_LATENCY_100NS / 10,
        "the budget is L in whole µs"
    );
    assert_eq!(TIMING_WARN_EVERY_100NS, 5 * UNITS_PER_SECOND, "5 s");
    assert_eq!(
        i64::from(TIMING_BUCKET_BOUNDARIES),
        60 * GENLOCK_GRID_FPS,
        "60 s of boundaries"
    );
}

#[test]
fn us_after_is_whole_microseconds_and_never_negative() {
    assert_eq!(us_after(100, 12_345), 1_224, "12 245 × 100 ns");
    assert_eq!(us_after(0, 9), 0, "under a µs");
    assert_eq!(us_after(5_000, 4_000), 0, "before its reference: 0 late");
    assert_eq!(
        us_after(i64::MIN, i64::MAX),
        922_337_203_685_477_580,
        "saturated, never an overflow"
    );
}

#[test]
fn a_boundarys_figures_are_its_take_and_hand_off_vs_the_boundary_and_the_ndi_call() {
    let marks = BoundaryMarks {
        stamp_100ns: B,
        taken_100ns: B + 12_345,
        fed_100ns: B + 23_456,
        submit_start_100ns: B + 30_000,
        submitted_100ns: B + 95_001,
    };
    assert_eq!(
        BoundarySample::of(&marks),
        BoundarySample {
            ready_late_us: 1_234,
            vban_feed_late_us: 2_345,
            submit_us: 6_500,
        }
    );
    let early = BoundaryMarks {
        taken_100ns: B - 5,
        fed_100ns: B - 1,
        ..marks
    };
    assert_eq!(
        BoundarySample::of(&early),
        BoundarySample {
            ready_late_us: 0,
            vban_feed_late_us: 0,
            submit_us: 6_500,
        },
        "taken and handed over before the boundary: 0 late"
    );
}

#[test]
fn worst_takes_each_figures_own_max() {
    let a = BoundarySample {
        ready_late_us: 1,
        vban_feed_late_us: 20,
        submit_us: 3,
    };
    let b = BoundarySample {
        ready_late_us: 10,
        vban_feed_late_us: 2,
        submit_us: 30,
    };
    let want = BoundarySample {
        ready_late_us: 10,
        vban_feed_late_us: 20,
        submit_us: 30,
    };
    assert_eq!(a.worst(b), want);
    assert_eq!(b.worst(a), want);
}

#[test]
fn the_maxima_cover_the_bucket_being_filled_and_the_last_full_one() {
    let mut t = BoundaryTiming::default();
    let small = figures(100, 100, 100);
    assert_eq!(t.observe(&figures(7_000, 9_000, 8_000)), None);
    for _ in 0..TIMING_BUCKET_BOUNDARIES - 2 {
        t.observe(&small);
    }
    let worst = |t: &BoundaryTiming| {
        let s = t.status();
        (
            s.ready_late_us_max,
            s.vban_feed_late_us_max,
            s.submit_us_max,
        )
    };
    assert_eq!(worst(&t), (7_000, 9_000, 8_000), "1799: one bucket");
    t.observe(&small);
    assert_eq!(
        worst(&t),
        (7_000, 9_000, 8_000),
        "1800: the full bucket is kept as the last one"
    );
    for _ in 0..TIMING_BUCKET_BOUNDARIES - 1 {
        t.observe(&small);
    }
    assert_eq!(
        worst(&t),
        (7_000, 9_000, 8_000),
        "3599: still the last full bucket"
    );
    t.observe(&small);
    assert_eq!(
        worst(&t),
        (100, 100, 100),
        "3600: the slow boundary is two buckets back — gone"
    );
    assert_eq!(
        t.status(),
        BoundaryTimingStatus {
            boundaries: 3_600,
            ready_late_us_max: 100,
            vban_feed_late_us_max: 100,
            submit_us_max: 100,
            ready_late_over_5ms: 1,
            vban_feed_late_over_5ms: 1,
            submit_over_5ms: 1,
            vban_feed_late_over_10ms: 0,
            vban_feed_late_over_budget: 0,
            warned: 0,
        },
        "the counts are since start"
    );
}

#[test]
fn a_figure_is_slow_strictly_over_5_ms_and_counts_on_its_own() {
    let mut t = BoundaryTiming::default();
    t.observe(&figures(5_000, 5_000, 5_000));
    let s = t.status();
    assert_eq!(
        (
            s.ready_late_over_5ms,
            s.vban_feed_late_over_5ms,
            s.submit_over_5ms
        ),
        (0, 0, 0),
        "exactly 5 ms is not over"
    );
    t.observe(&figures(5_001, 0, 0));
    t.observe(&figures(0, 5_001, 0));
    t.observe(&figures(0, 5_001, 0));
    t.observe(&figures(0, 0, 5_001));
    t.observe(&figures(0, 0, 5_001));
    t.observe(&figures(0, 0, 5_001));
    let s = t.status();
    assert_eq!(
        (
            s.ready_late_over_5ms,
            s.vban_feed_late_over_5ms,
            s.submit_over_5ms,
            s.boundaries
        ),
        (1, 2, 3, 7)
    );
}

#[test]
fn a_hand_off_inside_vbans_send_latency_is_counted_but_never_warned() {
    // #210 part 2, box 30.9.2026: hand-offs 10–33 ms late are the program's
    // normal state (41 % of the boundaries over 10 ms in the 15 min capture of
    // finding 5915907311, 67 834 of 105 105 since start in the lane's read,
    // comment 5916271682), and each one still reaches VBAN before its block's
    // first packet is due, L = 66.7 ms after the boundary. The part 1 WARN
    // over 10 ms fired every 5 s for nothing.
    let mut t = BoundaryTiming::default();
    for (k, feed_us) in [10_000, 10_001, 27_578, 66_666].into_iter().enumerate() {
        // 6 s apart: the rate limit never holds one back.
        let stamp = B + k as i64 * 6 * UNITS_PER_SECOND;
        assert_eq!(
            t.observe(&fed_late(stamp, feed_us)),
            None,
            "{feed_us} µs: inside VBAN's budget"
        );
    }
    let s = t.status();
    assert_eq!(
        (
            s.vban_feed_late_over_5ms,
            s.vban_feed_late_over_10ms,
            s.vban_feed_late_over_budget,
            s.warned
        ),
        (4, 3, 0, 0),
        "exactly 10 ms is not over 10 ms; exactly L is not over the budget"
    );
}

#[test]
fn a_hand_off_after_its_first_packet_was_due_is_warned_at_most_once_per_5_s_with_the_skipped_count()
{
    let mut t = BoundaryTiming::default();
    let b2 = B + 333_333;
    assert_eq!(
        t.observe(&fed_late(B, 66_666)),
        None,
        "exactly L: the first packet is not late yet"
    );
    assert_eq!(t.status().vban_feed_late_over_budget, 0);
    assert_eq!(
        t.observe(&fed_late(b2, 66_667)),
        Some(LateBoundary {
            stamp_100ns: b2,
            sample: BoundarySample {
                ready_late_us: 0,
                vban_feed_late_us: 66_667,
                submit_us: 0,
            },
            suppressed: 0,
        }),
        "the first one handed over after its first packet was due"
    );
    let second = b2 + UNITS_PER_SECOND;
    assert_eq!(t.observe(&fed_late(second, 100_000)), None, "1 s later");
    assert_eq!(
        t.observe(&fed_late(b2 + TIMING_WARN_EVERY_100NS - 1, 70_000)),
        None,
        "just under 5 s later"
    );
    let third = b2 + TIMING_WARN_EVERY_100NS;
    assert_eq!(
        t.observe(&fed_late(third, 80_000)),
        Some(LateBoundary {
            stamp_100ns: third,
            sample: BoundarySample {
                ready_late_us: 0,
                vban_feed_late_us: 80_000,
                submit_us: 0,
            },
            suppressed: 2,
        }),
        "5 s after the last WARN: warned, with the two it skipped"
    );
    assert_eq!(
        t.observe(&fed_late(third + UNITS_PER_SECOND, 30_000)),
        None,
        "inside the budget: neither warned nor skipped"
    );
    let fourth = third + 6 * UNITS_PER_SECOND;
    assert_eq!(
        t.observe(&fed_late(fourth, 67_000)),
        Some(LateBoundary {
            stamp_100ns: fourth,
            sample: BoundarySample {
                ready_late_us: 0,
                vban_feed_late_us: 67_000,
                submit_us: 0,
            },
            suppressed: 0,
        }),
        "6 s later: warned, nothing skipped since the last WARN"
    );
    assert_eq!(
        t.status(),
        BoundaryTimingStatus {
            boundaries: 7,
            ready_late_us_max: 0,
            vban_feed_late_us_max: 100_000,
            submit_us_max: 0,
            ready_late_over_5ms: 0,
            vban_feed_late_over_5ms: 7,
            submit_over_5ms: 0,
            vban_feed_late_over_10ms: 7,
            vban_feed_late_over_budget: 5,
            warned: 3,
        }
    );
}

#[test]
fn utc_label_is_utc_with_milliseconds() {
    assert_eq!(utc_label(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(utc_label(B), "2026-09-30T14:05:31.133Z");
}
