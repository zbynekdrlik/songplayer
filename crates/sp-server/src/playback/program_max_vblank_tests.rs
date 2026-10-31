//! #223 follow-up: the sends on the wall's refresh grid
//! (`program_max_vblank.rs`): the slot arithmetic, the phase after a
//! vblank, and the pacer's slot keeping, re-pick and hysteresis; #251: the
//! compose offset K and the compose instant at its phase of the grid,
//! whatever the offers' drift. The leads come from a scratch model of the
//! pacer.
//! Wired via `#[cfg(test)] #[path = "program_max_vblank_tests.rs"] mod tests;`.

use std::time::{Duration, Instant};

use sp_gpu::{VblankGrid, VblankState};

use super::{
    COMPOSE_BUDGET, Due, LEAD_HYSTERESIS, Slots, VblankLog, VblankPacer, compose_offset,
    phase_after_vblank, phase_after_vblank_us, shifted, signed_ns, vblank_log,
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

fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}

/// #251: the compose runs at least 14 ms before its send; the window's
/// top is K + one period + 3 ms.
#[test]
fn the_compose_budget_is_fourteen_ms_and_the_hysteresis_three() {
    assert_eq!((COMPOSE_BUDGET, LEAD_HYSTERESIS), (ms(14), ms(3)));
}

/// #251: K runs from the compose phase FORWARD to the send phase, plus
/// whole periods until it is at least the budget; both phases modulo the
/// period. SNV (send 19 ms = 2.333 ms into a 16.667 ms refresh, compose
/// 13 ms): 6 ms + one period; the default send phase 14 ms: 1 ms + one
/// period.
#[test]
fn the_compose_offset_runs_from_the_compose_phase_to_the_send_phase() {
    let p = ns(16_666_667);
    let k = |period, send, compose| compose_offset(period, send, compose, COMPOSE_BUDGET);
    assert_eq!(k(p, ms(19), ms(13)), ns(22_666_667), "SNV");
    assert_eq!(k(p, ms(14), ms(13)), ns(17_666_667), "the default");
    assert_eq!(k(ms(50), ms(40), ms(10)), ms(30), "30 ms: no period added");
    assert_eq!(k(ms(50), ms(24), ms(10)), ms(14), "exactly the budget");
    assert_eq!(k(ms(4), ms(1), ms(2)), ms(15), "3 ms + three periods");
    assert_eq!(k(p, ms(19), ns(29_666_667)), ns(22_666_667), "13 ms modulo");
    // A compose phase past the send phase plus a period: only its modulo
    // keeps the subtraction from underflowing (CI 38660773271's survivor).
    assert_eq!(k(p, ms(2), ms(30)), ns(22_000_001), "13.333 ms modulo");
    assert_eq!(
        k(ms(16), ms(13), ms(13)),
        ms(16),
        "the same phase: a period"
    );
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
    assert_eq!(phase_after_vblank_us(&g, b + ms(21) + ns(7_999)), 5_007);
    assert_eq!(phase_after_vblank_us(&g, b - ms(3)), 13_000);
}

/// No grid: the constant lead, composed at once, never a re-pick.
#[test]
fn without_a_grid_a_boundary_goes_at_the_constant_lead() {
    let b = base();
    let mut pacer = VblankPacer::default();
    assert_eq!(
        pacer.due(b, None, ms(8), ms(13)),
        Due {
            at: b + ms(12),
            compose_at: None,
            repicked: false
        }
    );
}

/// Slots every 16 ms, 8 ms after each vblank, compose phase 10 ms:
/// K = (8 − 10) mod 16 = 14 ms. Against boundaries every 33.333 ms the lead
/// shrinks by 1.333 ms a boundary and is kept while it is at least 14 ms;
/// at 13.667 ms the first slot from 14 ms on is picked (29.667 ms), once.
/// Each boundary is composed K before its send.
#[test]
fn the_slot_is_kept_while_its_lead_stays_in_the_window_then_picked_anew() {
    let b = base();
    let g = grid(b - ms(1), ms(16));
    let mut pacer = VblankPacer::default();
    let mut leads = Vec::new();
    for i in 0..12u32 {
        let offered = b + Duration::from_nanos(33_333_333) * i;
        let due = pacer.due(offered, Some(g), ms(8), ms(10));
        let composed = due.compose_at.map(|at| at - offered);
        assert_eq!(composed, Some(due.at - offered - ms(14)), "boundary {i}");
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
            (29_666_669, true),
            (28_333_336, false),
            (27_000_003, false),
            (25_666_670, false),
            (24_333_337, false),
        ]
    );
}

/// Both edges of the window keep the slot (16 ms slots 12 ms after the
/// vblank, compose phase 0: K = 12 + 16 = 28 ms, the window 28 … 47 ms).
/// Boundaries every 31 ms grow the lead 1 ms a boundary: 47 ms is kept, at
/// 48 ms the first slot from 28 ms on (32 ms) is picked. Boundaries every
/// 33 ms shrink it: 28 ms is kept, at 27 ms the first slot from 28 ms on
/// (43 ms) is picked.
#[test]
fn both_edges_of_the_lead_window_keep_the_slot() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let first = pacer.due(b, Some(g), ms(12), ms(0));
    assert_eq!(
        (first.at, first.compose_at, first.repicked),
        (b + ms(28), Some(b), false)
    );
    let mut leads = Vec::new();
    for i in 1..=20u32 {
        let offered = b + ms(31) * i;
        let due = pacer.due(offered, Some(g), ms(12), ms(0));
        leads.push(((due.at - offered).as_millis(), due.repicked));
    }
    let expected: Vec<(u128, bool)> = (29..=47)
        .map(|lead| (lead, false))
        .chain([(32, true)])
        .collect();
    assert_eq!(leads, expected, "the top edge, 47 ms, is kept");

    let mut pacer = VblankPacer::default();
    let mut leads = Vec::new();
    for i in 0..7u32 {
        let offered = b - ms(5) + ms(33) * i;
        let due = pacer.due(offered, Some(g), ms(12), ms(0));
        leads.push(((due.at - offered).as_millis(), due.repicked));
    }
    let expected: Vec<(u128, bool)> = (28..=33)
        .rev()
        .map(|lead| (lead, false))
        .chain([(43, true)])
        .collect();
    assert_eq!(leads, expected, "the bottom edge, 28 ms = K, is kept");
}

/// With no drift (16.666 666 ms slots, boundaries two slots apart) and an
/// offer's jitter of ±0.2 ms sitting at the window's lower edge (send phase
/// 12.1 ms, compose phase 0.1 ms: K = 12 ms + one period = 28.667 ms), the
/// slot is picked anew ONCE (into the window's middle), never back and
/// forth.
#[test]
fn jitter_at_the_windows_edge_picks_a_new_slot_once() {
    let b = base();
    let period = Duration::from_nanos(16_666_666);
    let g = grid(b, period);
    let (send, compose) = (Duration::from_micros(12_100), Duration::from_micros(100));
    assert_eq!(
        compose_offset(period, send, compose, COMPOSE_BUDGET),
        ns(28_666_666)
    );
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
        let due = pacer.due(offered, Some(g), send, compose);
        repicks += u32::from(due.repicked);
        leads.push((due.at - offered).as_micros());
    }
    assert_eq!(repicks, 1, "{leads:?}");
    assert_eq!(&leads[..4], [28_966, 45_233, 45_633, 45_233]);
}

/// A boundary with no grid forgets the lead: the next grid starts afresh
/// at the first slot from K = 28 ms on (30 ms), not near the old lead
/// (16 ms slots 12 ms after the vblank, compose phase 0, boundaries every
/// 31 ms: the lead grew to 44 ms; kept, it would be 46 ms).
#[test]
fn a_boundary_without_a_grid_starts_the_next_grid_afresh() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let mut last = None;
    for i in 0..17u32 {
        let offered = b + ms(31) * i;
        last = Some(pacer.due(offered, Some(g), ms(12), ms(0)).at - offered);
    }
    assert_eq!(last, Some(ms(44)));
    let gap = b + ms(31) * 17;
    let off = pacer.due(gap, None, ms(12), ms(0));
    assert_eq!((off.at, off.compose_at), (gap + ms(12), None));
    let offered = b + ms(31) * 18;
    let again = pacer.due(offered, Some(g), ms(12), ms(0));
    assert_eq!(
        (again.at - offered, again.repicked),
        (ms(30), false),
        "afresh: the first slot from 28 ms on"
    );
}

/// A new phase moves the kept slot with it (the nearest slot of the new
/// grid), without a re-pick while the lead stays in the window, and the
/// compose with it: K = (8 − 10) mod 16 = 14 ms, then (10 − 10) + 16 =
/// 16 ms; both composes 10 ms after their vblank.
#[test]
fn a_new_phase_moves_the_slot_without_a_repick() {
    let b = base();
    let g = grid(b, ms(16));
    let mut pacer = VblankPacer::default();
    let first = pacer.due(b, Some(g), ms(8), ms(10));
    assert_eq!((first.at, first.compose_at), (b + ms(24), Some(b + ms(10))));
    let moved = pacer.due(b + ms(32), Some(g), ms(10), ms(10));
    assert_eq!(
        (moved.at, moved.compose_at, moved.repicked),
        (b + ms(58), Some(b + ms(42)), false)
    );
}

/// The phase-lucky rule (`rust-workspace.md`): the offers drift ±0.5 ms a
/// boundary against SNV's grid (16.666 667 ms, send phase 19 ms, compose
/// phase 13 ms), so over 80 boundaries they sweep every phase of the
/// refresh and the lead crosses its window twice each way. Every boundary
/// is still composed exactly 13 ms after a vblank and sent 2.333 ms after
/// one, K = 22.667 ms apart, never before its offer; each re-pick keeps
/// that.
#[test]
fn the_compose_starts_at_its_phase_whatever_the_offers_drift() {
    let b = base();
    let period = ns(16_666_667);
    let g = grid(b, period);
    for step in [33_833_333u64, 32_833_333] {
        let mut pacer = VblankPacer::default();
        let (mut repicks, mut phases) = (0, std::collections::BTreeSet::new());
        for i in 0..80u32 {
            let offered = b + ns(step) * i;
            phases.insert(phase_after_vblank(&g, offered).as_millis());
            let due = pacer.due(offered, Some(g), ms(19), ms(13));
            repicks += u32::from(due.repicked);
            let composed = due.compose_at.expect("on the grid");
            assert_eq!(phase_after_vblank(&g, composed), ms(13), "{step}: {i}");
            assert_eq!(phase_after_vblank(&g, due.at), ns(2_333_333), "{step}: {i}");
            assert_eq!(due.at - composed, ns(22_666_667), "{step}: {i}");
            assert!(composed >= offered, "{step}: {i} composed before its offer");
        }
        assert_eq!((repicks, phases.len()), (2, 17), "{step}");
    }
}

/// #243: the output's state is logged only where it turns `not_ticking`
/// (one WARN, from any other state or none) and where it leaves it (one
/// INFO); every other change and every repeat logs nothing.
#[test]
fn only_entering_and_leaving_not_ticking_is_logged() {
    use VblankState::{Measuring, NotTicking, Ticking};
    let stopped = Some(VblankLog::StoppedTicking);
    let again = Some(VblankLog::TicksAgain);
    for (before, now, logged) in [
        (None, NotTicking, stopped),
        (Some(Measuring), NotTicking, stopped),
        (Some(Ticking), NotTicking, stopped),
        (Some(NotTicking), NotTicking, None),
        (Some(NotTicking), Measuring, again),
        (Some(NotTicking), Ticking, again),
        (None, Measuring, None),
        (None, Ticking, None),
        (Some(Measuring), Ticking, None),
        (Some(Ticking), Measuring, None),
        (Some(Ticking), Ticking, None),
    ] {
        assert_eq!(vblank_log(before, now), logged, "{before:?} -> {now:?}");
    }
}
