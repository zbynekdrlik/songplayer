//! #257 lane 2: the panel's timeline — exact bucket boundaries, the bars to
//! scale, the shared axis's ticks.

use super::*;
use crate::audio_faults::{FaultEvent, LocalTime};

const NOW: i64 = 1_760_000_000_000; // 2025-10-09 08:53:20 UTC
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

fn event(ts_ms: i64, severity: &str) -> FaultEvent {
    FaultEvent {
        ts_ms,
        kind: "underrun".to_string(),
        severity: severity.to_string(),
        amount: 1,
    }
}

fn b(loss: u32, risk: u32) -> Bucket {
    Bucket { loss, risk }
}

#[test]
fn a_bucket_starts_at_the_ceiling_of_its_share_of_the_window() {
    // A span of 1000 ms in 60 buckets: 16.67 ms each.
    let start = |i| bucket_start(1_000, 2_000, 60, i);
    assert_eq!(
        [start(0), start(1), start(2), start(3), start(59), start(60)],
        [1_000, 1_017, 1_034, 1_050, 1_984, 2_000]
    );
    assert_eq!(
        bucket_start(0, 3_600_000, 60, 7),
        420_000,
        "1 h: whole minutes"
    );
}

#[test]
fn every_instant_of_the_window_lies_in_the_bucket_whose_bounds_hold_it() {
    let (from, to, n) = (1_000, 2_000, 60);
    for t in from..=to {
        let i = bucket_of(t, from, to, n).unwrap();
        assert!(bucket_start(from, to, n, i) <= t, "{t} in {i}");
        let next = bucket_start(from, to, n, i + 1);
        assert!(
            t < next || (i == n - 1 && t == to),
            "{t} in {i}, next {next}"
        );
    }
    assert_eq!(bucket_of(from, from, to, n), Some(0));
    assert_eq!(bucket_of(1_016, from, to, n), Some(0));
    assert_eq!(bucket_of(1_017, from, to, n), Some(1));
    assert_eq!(bucket_of(1_983, from, to, n), Some(58));
    assert_eq!(bucket_of(1_984, from, to, n), Some(59));
    assert_eq!(
        bucket_of(to, from, to, n),
        Some(59),
        "the window's end is its last bucket"
    );
}

#[test]
fn an_instant_outside_the_window_or_an_empty_window_has_no_bucket() {
    assert_eq!(bucket_of(999, 1_000, 2_000, 60), None);
    assert_eq!(bucket_of(2_001, 1_000, 2_000, 60), None);
    assert_eq!(bucket_of(1_500, 1_000, 2_000, 0), None);
    assert_eq!(bucket_of(1_000, 1_000, 1_000, 60), None);
    assert_eq!(bucket_of(1_000, 1_000, 900, 60), None);
}

#[test]
fn the_buckets_count_loss_and_risk_rows_and_nothing_else() {
    let events = [
        event(1_000, "loss"),
        event(1_000, "risk"),
        event(1_017, "risk"),
        event(1_500, "info"),
        event(999, "loss"),
        event(2_000, "loss"),
        event(2_000, "loss"),
    ];
    let got = buckets(&events, 1_000, 2_000, 60);
    let mut want = vec![Bucket::default(); 60];
    want[0] = b(1, 1);
    want[1] = b(0, 1);
    want[59] = b(2, 0);
    assert_eq!(got, want);
}

#[test]
fn the_scale_is_the_largest_bucket_of_every_row() {
    let rows = vec![vec![b(2, 1), b(0, 0)], vec![b(1, 1)]];
    assert_eq!(shared_max(&rows), 3);
    assert_eq!(shared_max(&[]), 0);
}

#[test]
fn a_bar_is_drawn_to_scale_and_one_fault_is_never_invisible() {
    let table = [
        (b(0, 0), 5, (0, 0)),
        (b(1, 0), 0, (0, 0)),
        (b(0, 5), 5, (0, 24)),
        (b(5, 0), 5, (24, 0)),
        (b(1, 2), 3, (8, 16)),
        (b(3, 1), 8, (9, 3)),
        (b(1, 0), 100, (2, 0)),
        (b(1, 99), 100, (2, 22)),
    ];
    for (bucket, max, heights) in table {
        assert_eq!(bar_heights(bucket, max), heights, "{bucket:?} of {max}");
    }
}

#[test]
fn the_bars_sit_in_their_slots() {
    assert_eq!(bar_x(0), 1);
    assert_eq!(bar_x(1), 11);
    assert_eq!(bar_x(59), 591);
}

#[test]
fn a_bars_tooltip_names_its_time_and_rows() {
    let start = LocalTime {
        day: 12,
        month: 10,
        hour: 14,
        minute: 0,
        second: 0,
    };
    let end = LocalTime {
        minute: 24,
        ..start
    };
    assert_eq!(
        bucket_tip(start, end, b(1, 0)),
        "12.10. 14:00 – 12.10. 14:24: 1 chyba, 0 rizík"
    );
    assert_eq!(
        bucket_tip(start, end, b(5, 3)),
        "12.10. 14:00 – 12.10. 14:24: 5 chýb, 3 riziká"
    );
}

#[test]
fn the_tick_step_follows_the_windows_length() {
    let table = [
        (HOUR, 600_000),
        (2 * HOUR, 600_000),
        (2 * HOUR + 1, 3_600_000),
        (12 * HOUR, 3_600_000),
        (12 * HOUR + 1, 10_800_000),
        (DAY, 10_800_000),
        (2 * DAY, 10_800_000),
        (2 * DAY + 1, DAY),
        (7 * DAY, DAY),
        (10 * DAY, DAY),
        (10 * DAY + 1, 432_000_000),
        (30 * DAY, 432_000_000),
    ];
    for (span, step) in table {
        assert_eq!(tick_step(span), step, "{span}");
    }
}

fn ticks(first: i64, step: i64, count: i64, days: bool) -> Vec<Tick> {
    (0..count)
        .map(|k| Tick {
            at_ms: first + k * step,
            days,
        })
        .collect()
}

#[test]
fn an_hours_ticks_fall_on_whole_local_10_minutes() {
    // 09:53:20 – 10:53:20 local (UTC+2): 10:00, 10:10 … 10:50.
    assert_eq!(
        axis_ticks(NOW - HOUR, NOW, 120),
        ticks(NOW - 3_200_000, 600_000, 6, false)
    );
}

#[test]
fn a_days_ticks_fall_on_whole_local_3_hours() {
    // 8.10. 10:53:20 – 9.10. 10:53:20 local (UTC+2): 12:00, 15:00 … 9:00.
    assert_eq!(
        axis_ticks(NOW - DAY, NOW, 120),
        ticks(NOW - 82_400_000, 10_800_000, 8, false)
    );
}

#[test]
fn a_weeks_ticks_fall_on_local_midnights_labelled_by_the_day() {
    // UTC+1: 3.10. 00:00 … 9.10. 00:00 local = 23:00 UTC the day before.
    assert_eq!(
        axis_ticks(NOW - 7 * DAY, NOW, 60),
        ticks(NOW - 554_000_000, DAY, 7, true)
    );
}

#[test]
fn a_months_ticks_fall_on_every_fifth_midnight() {
    // UTC: 14.9., 19.9., 24.9., 29.9., 4.10., 9.10.
    assert_eq!(
        axis_ticks(NOW - 30 * DAY, NOW, 0),
        ticks(NOW - 2_192_000_000, 5 * DAY, 6, true)
    );
}

#[test]
fn a_window_on_whole_ticks_shows_both_its_ends() {
    let from = NOW - 200_000; // 08:50:00 UTC
    assert_eq!(
        axis_ticks(from, from + HOUR, 0),
        ticks(from, 600_000, 7, false)
    );
}

#[test]
fn an_instant_sits_at_its_share_of_the_axis() {
    assert_eq!(axis_pct(1_500, 1_000, 2_000), 50.0);
    assert_eq!(axis_pct(1_250, 1_000, 2_000), 25.0);
    assert_eq!(axis_pct(2_000, 1_000, 2_000), 100.0);
    assert_eq!(axis_pct(1_000, 1_000, 1_000), 0.0, "an empty window");
    assert_eq!(axis_pct(1_000, 1_000, 900), 0.0);
}
