//! Tests for each target's part of one compose's draw (#251 step 2).

use std::time::{Duration, Instant};

use super::{ComposeStats, split_uyvy_part, target_draws_us};
use crate::ring_state::{Acquired, SlotDraw, SlotOpen};

const MS: Duration = Duration::from_millis(1);

#[test]
fn each_target_s_draw_is_its_own_part_of_the_compose() {
    let start = Instant::now();
    let done = [start + 3 * MS, start + 5 * MS + Duration::from_micros(250)];
    assert_eq!(
        target_draws_us(start, &done),
        [3_000, 2_250],
        "MAX from the first draw command, the FHD target from MAX's end"
    );
}

#[test]
fn a_target_finished_with_the_one_before_drew_nothing_of_its_own() {
    let start = Instant::now();
    let done = [start + 4 * MS, start + 4 * MS, start + 7 * MS];
    assert_eq!(target_draws_us(start, &done), [4_000, 0, 3_000]);
}

#[test]
fn an_instant_seen_before_the_one_before_reads_zero() {
    let start = Instant::now();
    let done = [start + 5 * MS, start + 4 * MS, start + 6 * MS];
    assert_eq!(
        target_draws_us(start, &done),
        [5_000, 0, 2_000],
        "never negative; the next counts from the last instant seen"
    );
}

#[test]
fn no_target_no_draw() {
    assert!(target_draws_us(Instant::now(), &[]).is_empty());
}

/// #279 P1a: the wait ends the targets' queries, then the UYVY pass's: the
/// part after the targets' is the pass's own.
#[test]
fn the_part_after_the_targets_is_the_uyvy_pass_s() {
    assert_eq!(
        split_uyvy_part(vec![3_000, 2_250, 400], 2),
        (vec![3_000, 2_250], Some(400))
    );
    assert_eq!(split_uyvy_part(vec![5, 6], 1), (vec![5], Some(6)));
}

#[test]
fn with_no_uyvy_pass_every_part_is_a_target_s() {
    assert_eq!(
        split_uyvy_part(vec![3_000, 2_250], 2),
        (vec![3_000, 2_250], None)
    );
    assert_eq!(split_uyvy_part(Vec::new(), 0), (Vec::new(), None));
}

/// #279 P2: each ring slot of a compose is counted once, by what the
/// compositor found and did: drawn, busy, a dead ring's / abandoned / a
/// failed acquire, not opened; the compositor's own lost device counts
/// nothing. Two of each, so every counter is pinned past its first step.
#[test]
fn each_ring_slot_is_counted_by_what_the_compositor_found_and_did() {
    let mut stats = ComposeStats::default();
    let held = SlotOpen::Acquired(Acquired::Held);
    let busy = SlotOpen::Acquired(Acquired::Busy);
    let abandoned = SlotOpen::Acquired(Acquired::Abandoned);
    let failed = SlotOpen::Acquired(Acquired::Failed(0x8007_0057));
    for (open, draw) in [
        (held, SlotDraw::Draw),
        (busy, SlotDraw::Black),
        (SlotOpen::Dead, SlotDraw::Gone),
        (abandoned, SlotDraw::KillRing),
        (failed, SlotDraw::KillRing),
        (SlotOpen::Failed, SlotDraw::KillRing),
        (failed, SlotDraw::DeviceLost),
        (SlotOpen::Failed, SlotDraw::DeviceLost),
        (held, SlotDraw::Draw),
        (busy, SlotDraw::Black),
        (SlotOpen::Failed, SlotDraw::KillRing),
    ] {
        stats.count_slot(open, draw);
    }
    assert_eq!(
        (
            stats.gpu_sides,
            stats.km_acquire_timeouts,
            stats.abandoned,
            stats.open_failures
        ),
        (2, 2, 3, 2)
    );
    assert_eq!(stats.gpu_opens, 0, "opening is not drawing");
    assert_eq!((stats.uploads, stats.upload_bytes), (0, 0));
}
