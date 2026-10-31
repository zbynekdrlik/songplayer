//! #279 P0: the upload window and its rates, on made-up instants.
//! Wired via `#[cfg(test)] #[path = "program_max_upload_tests.rs"] mod tests;`.

use std::time::{Duration, Instant};

use sp_gpu::ComposeStats;

use super::{UPLOAD_RATE_SPAN, Uploads, per_second, tenth};
use crate::playback::program_max::{MAX_STAT_WINDOW, MaxOut};

const MS: Duration = Duration::from_millis(1);
const SECOND: Duration = Duration::from_secs(1);

#[test]
fn the_rates_count_everything_after_the_oldest_over_the_time_since_it() {
    let t0 = Instant::now();
    let mut uploads = Uploads::default();
    uploads.push(t0, 5_000_000, 1);
    uploads.push(t0 + SECOND, 3_000_000, 1);
    uploads.push(t0 + 2 * SECOND, 4_000_000, 2);
    assert_eq!(
        uploads.rates(),
        (3.5, 1.5),
        "7 MB and 3 pictures after the first, in 2 s"
    );
}

#[test]
fn below_two_boundaries_or_over_no_time_the_rates_are_zero() {
    let t0 = Instant::now();
    let mut uploads = Uploads::default();
    assert_eq!(uploads.rates(), (0.0, 0.0), "nothing sent");
    uploads.push(t0, 12_000_000, 2);
    assert_eq!(uploads.rates(), (0.0, 0.0), "one boundary");
    uploads.push(t0, 12_000_000, 2);
    assert_eq!(uploads.rates(), (0.0, 0.0), "two at one instant");
}

/// 1000 boundaries 10 ms apart, boundary i uploading i MB: the window
/// keeps the last 900 (i = 100..=999), so i = 101..=999 over 8.99 s.
#[test]
fn the_window_keeps_the_last_900_boundaries() {
    assert_eq!(MAX_STAT_WINDOW, 900);
    let t0 = Instant::now();
    let mut uploads = Uploads::default();
    for i in 0..1_000u32 {
        uploads.push(t0 + 10 * MS * i, u64::from(i) * 1_000_000, 1);
    }
    // (101 + 999) · 899 / 2 = 494 450 MB in 8.99 s; 899 pictures.
    assert_eq!(uploads.rates(), (55_000.0, 100.0));
}

/// A boundary more than [`UPLOAD_RATE_SPAN`] older than the newest leaves
/// the window; one exactly that old stays.
#[test]
fn a_boundary_older_than_the_span_before_the_newest_leaves() {
    assert_eq!(UPLOAD_RATE_SPAN, 30 * SECOND);
    let t0 = Instant::now();
    let mut uploads = Uploads::default();
    uploads.push(t0, 9_000_000, 9);
    uploads.push(t0 + SECOND, 31_000_000, 31);
    uploads.push(t0 + 31 * SECOND, 60_000_000, 60);
    assert_eq!(
        uploads.rates(),
        (2.0, 2.0),
        "the boundary at 1 s stays (exactly 30 s old): 60 MB, 60 pictures in 30 s"
    );
    uploads.push(t0 + 31 * SECOND + MS, 0, 0);
    assert_eq!(
        uploads.rates(),
        (0.0, 0.0),
        "1 ms later it is 30.001 s old and leaves: nothing after the 31 s one"
    );
}

#[test]
fn a_rate_is_per_second_and_rounded_to_a_tenth() {
    assert_eq!(per_second(3, 2 * SECOND), 1.5);
    assert_eq!(per_second(3, 500 * MS), 6.0);
    assert_eq!(per_second(3, Duration::ZERO), 0.0, "no time, no rate");
    assert_eq!(tenth(93.312), 93.3);
    assert_eq!(tenth(0.04), 0.0);
    assert_eq!(tenth(0.06), 0.1);
    assert_eq!(tenth(2.0), 2.0);
}

/// Through `MaxOut`: `max.upload_mb_s` / `max.uploads_per_s` on the status.
#[test]
fn the_status_carries_the_rates() {
    let max = MaxOut::new();
    let status = max.status();
    assert_eq!((status.upload_mb_s, status.uploads_per_s), (0.0, 0.0));
    let t0 = Instant::now();
    max.record_uploads(t0, 12_441_600, 1);
    max.record_uploads(t0 + 40 * MS, 12_441_600, 1);
    max.record_uploads(t0 + 80 * MS, 0, 0);
    let status = max.status();
    assert_eq!(
        (status.upload_mb_s, status.uploads_per_s),
        (155.5, 12.5),
        "one 4K picture after the first in 80 ms"
    );
}

/// #279 P3a: a compose's sides by kind over the window, and its ring
/// slots' failures summed since the start (every compose's, the ones that
/// left the window included).
#[test]
fn a_compose_records_its_sides_and_its_slots_failures() {
    let t0 = Instant::now();
    let mut uploads = Uploads::default();
    assert_eq!(uploads.side_rates(), (0.0, 0.0));
    let fade = ComposeStats {
        uploads: 1,
        upload_bytes: 4_000_000,
        gpu_sides: 1,
        gpu_opens: 1,
        km_acquire_timeouts: 1,
        abandoned: 2,
        open_failures: 3,
        ..ComposeStats::default()
    };
    uploads.push_compose(t0, &fade, 1);
    uploads.push_compose(t0 + SECOND, &fade, 1);
    let two_rings = ComposeStats {
        gpu_sides: 2,
        ..ComposeStats::default()
    };
    uploads.push_compose(t0 + 2 * SECOND, &two_rings, 0);
    assert_eq!(
        uploads.side_rates(),
        (1.5, 0.5),
        "3 ring slots and 1 CPU picture after the first, in 2 s"
    );
    assert_eq!(uploads.rates(), (2.0, 0.5), "4 MB in 1 picture in 2 s");
    let slots = uploads.slots;
    assert_eq!(
        (
            slots.km_acquire_timeouts,
            slots.abandoned,
            slots.open_failures,
            slots.gpu_opens
        ),
        (2, 4, 6, 2),
        "two composes' failures, the first one's too"
    );
    uploads.push(t0 + 3 * SECOND, 0, 0);
    assert_eq!(
        uploads.side_rates(),
        (1.0, 0.3),
        "a plain upload record draws no side: 3 slots and 1 picture in 3 s"
    );
}
