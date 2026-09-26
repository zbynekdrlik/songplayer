//! #147 (design record 5845527884, Approach 1 (a)): the paced output's grid
//! bookkeeping. Exact boundaries on the 30 fps exact-rational grid, so every
//! comparison and every `± 1` in `PacedGrid` is pinned.

use super::*;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// A quarter of the 30 fps slot (333 333 / 4), in 100-ns units.
const GRACE: i64 = 83_333;

/// A grid whose last pacer serviced `b(last)` and then detached.
fn detached_after(last: i64) -> PacedGrid {
    let mut g = PacedGrid::new(30);
    assert_eq!(g.attach(), None);
    assert!(g.accept_job(b(last)));
    g.detach();
    g
}

#[test]
fn the_fill_grace_is_a_quarter_slot() {
    assert_eq!(fill_grace_100ns(30), GRACE);
    assert_eq!(fill_grace_100ns(60), 166_666 / 4);
    assert_eq!(fill_grace_100ns(0), 0, "genlock off: no grace");
}

#[test]
fn a_fresh_grid_waits_for_a_job_and_never_fills() {
    let g = PacedGrid::new(30);
    assert_eq!(g.step(0), GridStep::Idle);
    assert_eq!(g.step(b(100)), GridStep::Idle, "nothing serviced yet");
    assert_eq!(g.last_serviced_100ns(), None);
    assert!(!g.is_attached());
}

#[test]
fn a_detached_grid_fills_the_next_boundary_a_quarter_slot_after_it() {
    let mut g = detached_after(3);
    assert_eq!(
        g.step(b(4) + GRACE - 1),
        GridStep::WaitUntil(b(4) + GRACE),
        "one tick before the deadline: still waiting for a job"
    );
    assert_eq!(g.step(b(4)), GridStep::WaitUntil(b(4) + GRACE));
    assert_eq!(
        g.step(b(4) + GRACE),
        GridStep::Fill(b(4)),
        "at the deadline the next boundary is filled"
    );
    assert_eq!(g.commit_fill(b(4)), 0, "the next boundary skips nothing");
    assert_eq!(g.last_serviced_100ns(), Some(b(4)));
    assert_eq!(g.fill_pairs(), 1);
    assert_eq!(g.step(b(5) + GRACE), GridStep::Fill(b(5)));
    assert_eq!(g.commit_fill(b(5)), 0);
    assert_eq!(g.fill_pairs(), 2);
    assert_eq!(g.unserviced_slots(), 0);
}

#[test]
fn an_attached_grid_never_fills_however_late_the_pacer_is() {
    let mut g = PacedGrid::new(30);
    g.attach();
    assert!(g.accept_job(b(3)));
    assert!(g.is_attached());
    assert_eq!(g.step(b(4) + GRACE), GridStep::Idle);
    assert_eq!(g.step(b(40)), GridStep::Idle, "the pacer owns the grid");
    assert_eq!(g.fill_pairs(), 0);
}

#[test]
fn a_consumer_woken_eight_slots_late_catches_up_one_boundary_at_a_time() {
    let mut g = detached_after(3);
    // The next boundary b(4) is exactly 8 slots behind b(12): a catch-up.
    assert_eq!(g.step(b(12) + GRACE), GridStep::Fill(b(4)));
    assert_eq!(g.commit_fill(b(4)), 0);
    assert_eq!(g.step(b(12) + GRACE), GridStep::Fill(b(5)));
    assert_eq!(g.unserviced_slots(), 0);
}

#[test]
fn a_consumer_woken_more_than_eight_slots_late_resyncs_and_counts_the_hole() {
    let mut g = detached_after(3);
    // b(4) is 9 slots behind b(13): resync onto the current boundary.
    assert_eq!(g.step(b(13) + GRACE), GridStep::Fill(b(13)));
    assert_eq!(g.commit_fill(b(13)), 9, "b(4)..=b(12) went unserviced");
    assert_eq!(g.unserviced_slots(), 9);
    assert_eq!(g.fill_pairs(), 1);
    assert_eq!(fill_boundary(b(4), b(12) + GRACE, 30), b(4));
    assert_eq!(fill_boundary(b(4), b(13), 30), b(13));
}

#[test]
fn a_stale_or_duplicate_job_is_refused() {
    let mut g = PacedGrid::new(30);
    g.attach();
    assert!(g.accept_job(b(5)));
    assert!(!g.accept_job(b(5)), "a duplicate stamp is never sent twice");
    assert!(!g.accept_job(b(4)), "an earlier stamp is never sent");
    assert_eq!(g.last_serviced_100ns(), Some(b(5)));
    assert!(g.accept_job(b(6)));
    assert_eq!(g.last_serviced_100ns(), Some(b(6)));
}

#[test]
fn a_gap_counts_as_unserviced_only_across_a_detach_attach_window() {
    let mut g = PacedGrid::new(30);
    g.attach();
    assert!(g.accept_job(b(1)));
    assert!(g.accept_job(b(3)));
    assert_eq!(g.unserviced_slots(), 0, "an in-song gap is the pacer's");
    g.detach();
    assert_eq!(g.attach(), Some(b(3)));
    assert!(g.accept_job(b(6)));
    assert_eq!(
        g.unserviced_slots(),
        2,
        "b(4) + b(5) across the song change"
    );
    // The first job of the attached pacer closed the window.
    assert!(g.accept_job(b(9)));
    assert_eq!(g.unserviced_slots(), 2);
}

#[test]
fn attach_hands_the_next_pacer_the_last_serviced_stamp() {
    let mut g = detached_after(2);
    assert_eq!(g.step(b(3) + GRACE), GridStep::Fill(b(3)));
    g.commit_fill(b(3));
    assert_eq!(g.attach(), Some(b(3)), "a fill counts as serviced");
    assert!(g.accept_job(b(4)));
    assert_eq!(g.unserviced_slots(), 0);
}

#[test]
fn genlock_off_never_fills_or_counts() {
    let mut g = PacedGrid::new(0);
    g.attach();
    assert!(g.accept_job(10));
    g.detach();
    assert_eq!(g.step(1_000_000_000), GridStep::Idle);
    assert!(g.accept_job(20));
    assert_eq!(g.unserviced_slots(), 0);
}
