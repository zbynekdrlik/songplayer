//! #257 lane 2: the panel's timeline — exact bucket boundaries, the bars to
//! scale, the shared axis's ticks.

use super::*;
use crate::audio_faults::LocalTime;

const NOW: i64 = 1_760_000_000_000; // 2025-10-09 08:53:20 UTC
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

/// A zone at a fixed offset (minutes east of UTC).
fn fixed(minutes: i32) -> impl Fn(i64) -> i32 {
    move |_| minutes
}

/// Europe/Bratislava in 2026: CEST (+120) from 29.3. 01:00 UTC until 25.10.
/// 01:00 UTC, else CET (+60) — checked against the zone database.
const SPRING: i64 = 1_774_746_000_000;
const AUTUMN: i64 = 1_792_890_000_000;

fn bratislava(utc: i64) -> i32 {
    if (SPRING..AUTUMN).contains(&utc) {
        120
    } else {
        60
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

/// The server's bucket query numbers its rows by these edges: bucket i
/// = [edge i, edge i + 1), the same split [`bucket_of`] makes.
#[test]
fn the_inner_edges_are_the_buckets_first_instants() {
    let edges = bucket_edges(1_000, 2_000, 60);
    assert_eq!(edges.len(), 59);
    assert_eq!(
        [edges[0], edges[1], edges[2], edges[58]],
        [1_017, 1_034, 1_050, 1_984]
    );
    for (i, edge) in edges.iter().enumerate() {
        assert_eq!(*edge, bucket_start(1_000, 2_000, 60, i + 1), "{i}");
        assert_eq!(bucket_of(*edge, 1_000, 2_000, 60), Some(i + 1));
        assert_eq!(bucket_of(*edge - 1, 1_000, 2_000, 60), Some(i));
    }
}

#[test]
fn a_paths_buckets_read_its_answer_and_pad_with_zeros() {
    let got = buckets_of(&[1, 0, 2], &[0, 3]);
    assert_eq!(got.len(), BUCKETS);
    assert_eq!(&got[..3], &[b(1, 0), b(0, 3), b(2, 0)]);
    assert!(got[3..].iter().all(|x| *x == Bucket::default()));
    let mut long = vec![0; BUCKETS + 5];
    long[BUCKETS - 1] = 7;
    long[BUCKETS] = 9;
    let got = buckets_of(&long, &[]);
    assert_eq!(got.len(), BUCKETS);
    assert_eq!(
        got[BUCKETS - 1],
        b(7, 0),
        "never more than the timeline's buckets"
    );
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
        axis_ticks(NOW - HOUR, NOW, &fixed(120)),
        ticks(NOW - 3_200_000, 600_000, 6, false)
    );
}

#[test]
fn a_days_ticks_fall_on_whole_local_3_hours() {
    // 8.10. 10:53:20 – 9.10. 10:53:20 local (UTC+2): 12:00, 15:00 … 9:00.
    assert_eq!(
        axis_ticks(NOW - DAY, NOW, &fixed(120)),
        ticks(NOW - 82_400_000, 10_800_000, 8, false)
    );
}

#[test]
fn a_weeks_ticks_fall_on_local_midnights_labelled_by_the_day() {
    // UTC+1: 3.10. 00:00 … 9.10. 00:00 local = 23:00 UTC the day before.
    assert_eq!(
        axis_ticks(NOW - 7 * DAY, NOW, &fixed(60)),
        ticks(NOW - 554_000_000, DAY, 7, true)
    );
}

#[test]
fn a_months_ticks_fall_on_every_fifth_midnight() {
    // UTC: 14.9., 19.9., 24.9., 29.9., 4.10., 9.10.
    assert_eq!(
        axis_ticks(NOW - 30 * DAY, NOW, &fixed(0)),
        ticks(NOW - 2_192_000_000, 5 * DAY, 6, true)
    );
}

#[test]
fn a_window_on_whole_ticks_shows_both_its_ends() {
    let from = NOW - 200_000; // 08:50:00 UTC
    assert_eq!(
        axis_ticks(from, from + HOUR, &fixed(0)),
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

/// #257: a planned restart is no fault, so a start is a mark on its
/// timeline, never a bar; only the starts inside the window are marked.
#[test]
fn every_start_inside_the_window_is_a_mark_at_its_place() {
    let starts = [999, 1_000, 1_500, 2_000, 2_001];
    let marks = start_marks(&starts, 1_000, 2_000);
    assert_eq!(
        marks,
        vec![
            StartMark {
                at_ms: 1_000,
                pct: 0.0
            },
            StartMark {
                at_ms: 1_500,
                pct: 50.0
            },
            StartMark {
                at_ms: 2_000,
                pct: 100.0
            },
        ]
    );
}

#[test]
fn a_start_marks_tooltip_names_its_local_time() {
    let at = LocalTime {
        day: 12,
        month: 10,
        hour: 14,
        minute: 5,
        second: 7,
    };
    assert_eq!(start_tip(at), "spustenie 12.10. 14:05:07");
}

fn day_ticks(first: i64, offsets: &[i64]) -> Vec<Tick> {
    offsets
        .iter()
        .map(|o| Tick {
            at_ms: first + o,
            days: true,
        })
        .collect()
}

/// #257 review: a day tick is snapped with the zone's offset AT that tick,
/// so every one is a local midnight across the spring change (29.3.).
#[test]
fn day_ticks_stay_on_local_midnights_across_the_spring_change() {
    let from = 1_774_526_400_000; // 26.3. 12:00 UTC
    assert_eq!(
        axis_ticks(from, from + 7 * DAY, &bratislava),
        day_ticks(
            from,
            &[
                39_600_000,
                126_000_000,
                212_400_000,
                295_200_000,
                381_600_000,
                468_000_000,
                554_400_000
            ]
        ),
        "27.3. … 2.4. 00:00 local: 23:00 UTC before the change, 22:00 UTC after"
    );
}

#[test]
fn day_ticks_stay_on_local_midnights_across_the_autumn_change() {
    let from = 1_792_584_000_000; // 21.10. 12:00 UTC
    assert_eq!(
        axis_ticks(from, from + 7 * DAY, &bratislava),
        day_ticks(
            from,
            &[
                36_000_000,
                122_400_000,
                208_800_000,
                295_200_000,
                385_200_000,
                471_600_000,
                558_000_000
            ]
        ),
        "22.10. … 28.10. 00:00 local: 22:00 UTC before the change, 23:00 UTC after"
    );
}

/// Hour ticks across the changes: the skipped local hour (2:00 in spring)
/// gives no tick of its own, the repeated one (2:00 in autumn) one tick.
#[test]
fn hour_ticks_across_the_changes_name_each_local_hour_once() {
    let spring = SPRING - 4 * HOUR - 1_800_000;
    let hours: Vec<i64> = axis_ticks(spring, SPRING + 6 * HOUR, &bratislava)
        .iter()
        .map(|t| t.at_ms - spring)
        .collect();
    assert_eq!(
        hours,
        [
            1_800_000, 5_400_000, 9_000_000, 12_600_000, 16_200_000, 19_800_000, 23_400_000,
            27_000_000, 30_600_000, 34_200_000, 37_800_000
        ],
        "22:00 … 1:00, then 3:00 … 9:00 local"
    );
    let autumn = AUTUMN - 4 * HOUR - 1_800_000;
    let hours: Vec<i64> = axis_ticks(autumn, AUTUMN + 6 * HOUR, &bratislava)
        .iter()
        .map(|t| t.at_ms - autumn)
        .collect();
    assert_eq!(
        hours,
        [
            1_800_000, 5_400_000, 9_000_000, 12_600_000, 19_800_000, 23_400_000, 27_000_000,
            30_600_000, 34_200_000, 37_800_000
        ],
        "23:00 … 2:00 summer time, then 3:00 … 8:00 winter time"
    );
}

/// 10-minute ticks across the spring change: 2:00 … 2:50 local do not
/// exist (they read as 1:00 … 1:50 again), so 1:50 is followed by 3:00
/// local, 10 minutes later — never a tick back in time.
#[test]
fn ten_minute_ticks_across_the_spring_change_only_move_forward() {
    let from = SPRING - 1_800_000; // 1:30 local (CET)
    let at: Vec<i64> = axis_ticks(from, SPRING + 1_800_000, &bratislava)
        .iter()
        .map(|t| t.at_ms - from)
        .collect();
    assert_eq!(
        at,
        [
            0, 600_000, 1_200_000, 1_800_000, 2_400_000, 3_000_000, 3_600_000
        ],
        "1:30, 1:40, 1:50 CET, then 3:00 … 3:30 CEST"
    );
}

#[test]
fn a_marks_place_is_a_whole_unit_of_the_timeline() {
    assert_eq!(mark_x(0.0), 0);
    assert_eq!(mark_x(50.0), 300);
    assert_eq!(mark_x(100.0), 600);
    assert_eq!(mark_x(12.5), 75);
    assert_eq!(mark_x(0.09), 1, "0.54 rounds up");
    assert_eq!(mark_x(0.08), 0, "0.48 rounds down");
}
