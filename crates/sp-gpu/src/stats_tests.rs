//! Tests for each target's part of one compose's draw (#251 step 2).

use std::time::{Duration, Instant};

use super::{split_uyvy_part, target_draws_us};

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
