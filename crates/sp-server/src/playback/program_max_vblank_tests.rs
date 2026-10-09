//! #223 follow-up: the sends on the wall's refresh grid
//! (`program_max_vblank.rs`): the slot arithmetic, the phase after a
//! vblank, and the pacer's slot keeping, re-pick and hysteresis. The leads
//! come from a scratch model of the pacer.
//! Wired via `#[cfg(test)] #[path = "program_max_vblank_tests.rs"] mod tests;`.

use std::time::{Duration, Instant};

use sp_gpu::VblankGrid;

use super::{
    Due, LEAD_HYSTERESIS, LEAD_MIN, Slots, VblankPacer, phase_after_vblank, shifted, signed_ns,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// An instant well after the process start, so the tests can go back from
/// it.
fn base() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn grid(at: Instant, period: Duration) -> VblankGrid {
    VblankGrid { at, period }
}

#[test]
fn the_lead_window_is_twelve_ms_plus_a_period_plus_three() {
    assert_eq!((LEAD_MIN, LEAD_HYSTERESIS), (ms(12), ms(3)));
}

#[test]
fn signed_nanoseconds_and_shifts_go_both_ways() {
    let b = base();
    assert_eq!(signed_ns(b + ms(3), b), 3_000_000);
    assert_eq!(signed_ns(b - ms(3), b), -3_000_000);
    assert_eq!(signed_ns(b, b), 0);
    assert_eq!(shifted(b, 2_000_000), b + ms(2));
    assert_eq!(shifted(b, -2_000_000), b - ms(2));
    assert_eq!(shifted(b, 0), b);
}

/// Slots every 16 ms from 5 ms after the vblank (5 = not half the period:
/// a phase applied backwards would land elsewhere).
#[test]
fn the_nearest_slot_on_either_side_and_a_tie_takes_the_later() {
    let b = base();
    let slots = Slots::new(&grid(b, ms(16)), ms(5));
    assert_eq!(slots.nearest(b + ms(12)), b + ms(5), "7 ms after");
    assert_eq!(slots.nearest(b + ms(14)), b + ms(21), "7 ms before");
    assert_eq!(slots.nearest(b - ms(2)), b + ms(5), "7 ms before");
    assert_eq!(slots.nearest(b - ms(4)), b - ms(11), "7 ms after");
    assert_eq!(slots.nearest(b + ms(13)), b + ms(21), "a tie");
}

#[test]
fn the_first_slot_at_or_after_an_instant_includes_it() {
    let b = base();
    let slots = Slots::new(&grid(b, ms(16)), ms(5));
    assert_eq!(slots.at_or_after(b + ms(5)), b + ms(5));
    assert_eq!(
        slots.at_or_after(b + ms(5) + Duration::from_nanos(1)),
        b + ms(21)
    );
    assert_eq!(slots.at_or_after(b - ms(3)), b + ms(5));
    assert_eq!(slots.at_or_after(b - ms(12)), b - ms(11));
}

#[test]
fn the_phase_after_a_vblank_is_within_one_period() {
    let b = base();
    let g = grid(b, ms(16));
    assert_eq!(phase_after_vblank(&g, b + ms(21)), ms(5));
    assert_eq!(phase_after_vblank(&g, b - ms(3)), ms(13));
    assert_eq!(phase_after_vblank(&g, b), Duration::ZERO);
    assert_eq!(phase_after_vblank(&g, b + ms(16)), Duration::ZERO);
}

/// No grid: the constant lead, never a re-pick.
#[test]
fn without_a_grid_a_boundary_goes_at_the_constant_lead() {
    let b = base();
    let mut pacer = VblankPacer::default();
    assert_eq!(
        pacer.due(b, None, ms(8)),
        Due {
            at: b + ms(12),
            repicked: false
        }
    );
}

/// Slots every 16 ms against boundaries every 33.333 ms: the lead shrinks
/// by 1.333 ms a boundary and is kept while it is at least 12 ms; at
/// 11 ms the first slot from 12 ms on is picked (27 ms), once.
#[test]
fn the_slot_is_kept_while_its_lead_stays_in_the_window_then_picked_anew() {
    let b = base();
    let g = grid(b - ms(1), ms(16));
    let mut pacer = VblankPacer::default();
    let mut leads = Vec::new();
    for i in 0..12u32 {
        let offered = b + Duration::from_nanos(33_333_333) * i;
        let due = pacer.due(offered, Some(g), ms(8));
        leads.push(((due.at - offered).as_nanos(), due.repicked));
    }
    assert_eq!(
        leads,
        [
            (23_000_000, false),
            (21_666_667, false),
            (20_333_334, false),
            (19_000_001, false),
            (17_666_668, false),
            (16_333_335, false),
            (15_000_002, false),
            (13_666_669, false),
            (12_333_336, false),
            (27_000_003, true),
            (25_666_670, false),
            (24_333_337, false),
        ]
    );
}

/// Both edges of the window keep the slot: a lead of exactly 12 ms, and
/// one of exactly 12 + 16 + 3 = 31 ms (boundaries every 31 ms against 16 ms
/// slots: the lead grows 1 ms a boundary); at 32 ms the first slot from
/// 12 ms on (16 ms) is picked.
#[test]
fn both_edges_of_the_lead_window_keep_the_slot() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let first = pacer.due(b, Some(g), ms(12));
    assert_eq!((first.at, first.repicked), (b + ms(12), false));
    let mut leads = Vec::new();
    for i in 1..=20u32 {
        let offered = b + ms(31) * i;
        let due = pacer.due(offered, Some(g), ms(12));
        leads.push(((due.at - offered).as_millis(), due.repicked));
    }
    let expected: Vec<(u128, bool)> = (13..=31)
        .map(|lead| (lead, false))
        .chain([(16, true)])
        .collect();
    assert_eq!(leads, expected);
}

/// With no drift (16.666 666 ms slots, boundaries two slots apart) and an
/// offer's jitter of ±0.2 ms sitting at the window's lower edge, the slot is
/// picked anew ONCE (into the window's middle), never back and forth.
#[test]
fn jitter_at_the_windows_edge_picks_a_new_slot_once() {
    let b = base();
    let period = Duration::from_nanos(16_666_666);
    let g = grid(b, period);
    let phase = Duration::from_micros(12_100);
    let mut pacer = VblankPacer::default();
    let mut repicks = 0;
    let mut leads = Vec::new();
    for i in 0..20u32 {
        let slot_pair = b + period * (2 * i);
        let offered = if i.is_multiple_of(2) {
            slot_pair - Duration::from_micros(200)
        } else {
            slot_pair + Duration::from_micros(200)
        };
        let due = pacer.due(offered, Some(g), phase);
        repicks += u32::from(due.repicked);
        leads.push((due.at - offered).as_micros());
    }
    assert_eq!(repicks, 1, "{leads:?}");
    assert_eq!(&leads[..4], [12_300, 28_566, 28_966, 28_566]);
}

/// A boundary with no grid forgets the lead: the next grid starts afresh
/// at the first slot from 12 ms on (14 ms), not near the old lead (16 ms
/// slots, boundaries every 31 ms: the lead grew to 28 ms; kept, it would be
/// 30 ms).
#[test]
fn a_boundary_without_a_grid_starts_the_next_grid_afresh() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let mut last = None;
    for i in 0..17u32 {
        let offered = b + ms(31) * i;
        last = Some(pacer.due(offered, Some(g), ms(12)).at - offered);
    }
    assert_eq!(last, Some(ms(28)));
    let gap = b + ms(31) * 17;
    assert_eq!(pacer.due(gap, None, ms(12)).at, gap + ms(12));
    let offered = b + ms(31) * 18;
    let again = pacer.due(offered, Some(g), ms(12));
    assert_eq!(
        (again.at - offered, again.repicked),
        (ms(14), false),
        "afresh: the first slot from 12 ms on"
    );
}

/// A new phase moves the kept slot with it (the nearest slot of the new
/// grid), without a re-pick while the lead stays in the window.
#[test]
fn a_new_phase_moves_the_slot_without_a_repick() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let first = pacer.due(b, Some(g), ms(8));
    assert_eq!(first.at, b + ms(24));
    let moved = pacer.due(b + ms(32), Some(g), ms(10));
    assert_eq!((moved.at, moved.repicked), (b + ms(58), false));
}
