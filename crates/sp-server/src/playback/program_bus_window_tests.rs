//! #257: `health.filled_owned` — a fill of a REAL source's boundary (a
//! playlist or the NDI input), and a fade's missing real side: the audio
//! that source should have had went missing, which the audio fault ledger
//! records as a loss; likewise `health.resyncs_owned`, a resync over a real
//! source's boundaries. Blank's fills and those of nothing selected are
//! silence by design and never count. Reuses the #209 rig
//! (`program_bus_tests.rs`) and its #215 fade (`program_bus_tests_transition.rs`).
//! Wired via `#[cfg(test)] #[path = "program_bus_window_tests.rs"] mod tests;`
//! in `program_bus_window.rs`.

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID};

use super::super::tests::{MS, SRC_A, SRC_B, b, frame, grace, job};
use super::super::tests_transition::{fade_300, offer_both};
use super::super::{ProgramCore, ProgramHealth};
use super::real_source;

#[test]
fn a_real_source_is_a_playlist_or_the_ndi_input_never_blank_or_nothing() {
    assert!(real_source(Some(SRC_A)));
    assert!(real_source(Some(PROGRAM_INPUT_ID)));
    assert!(!real_source(Some(PROGRAM_BLANK_ID)));
    assert!(!real_source(None));
}

/// `selected` (if any) offered nothing: the sender's wall fills b(1), once.
fn one_fill(selected: Option<i64>) -> ProgramHealth {
    let mut core = ProgramCore::new();
    if let Some(pid) = selected {
        core.select_initial(pid);
    }
    core.release(b(1) + MS);
    core.status().health
}

#[test]
fn a_fill_is_owned_only_on_a_real_sources_boundary() {
    let cases = [
        (Some(SRC_A), 1),
        (Some(PROGRAM_INPUT_ID), 1),
        (Some(PROGRAM_BLANK_ID), 0),
        (None, 0),
    ];
    for (selected, owned) in cases {
        let h = one_fill(selected);
        assert_eq!((h.filled, h.filled_owned), (1, owned), "{selected:?}");
    }
}

/// A 300 ms fade from `outgoing` to B, its cue still waiting on b(7) (B's
/// pair there is its pre-roll, not live) while `outgoing` sent nothing for
/// b(7): the boundary is HELD on `outgoing` and filled.
fn held_fill(outgoing: i64) -> ProgramHealth {
    let mut core = ProgramCore::new();
    core.select_initial(outgoing);
    assert!(core.set_transition(fade_300()));
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    if outgoing == SRC_A {
        for k in 1..=6 {
            core.offer(SRC_A, job(4, &fa, b(k), 0.1));
            if k == 5 {
                assert!(core.cut(SRC_B, b(5) + 5 * MS));
            }
        }
    } else {
        assert!(core.cut(SRC_B, b(5) + 5 * MS));
    }
    let mut preroll = job(8, &fb, b(7), 0.2);
    preroll.live = false;
    core.offer(SRC_B, preroll);
    core.release(b(7) + grace() + MS);
    core.status().health
}

#[test]
fn a_held_boundary_whose_playlist_sent_nothing_is_an_owned_fill() {
    let h = held_fill(SRC_A);
    assert_eq!((h.filled, h.filled_owned), (1, 1));
}

#[test]
fn a_held_boundary_out_of_blank_is_a_fill_by_design() {
    let h = held_fill(PROGRAM_BLANK_ID);
    assert_eq!((h.filled, h.filled_owned), (1, 0));
}

/// A running fade A → B: b(7) and b(8) mixed from both sides, then A sends
/// nothing for b(9) and its grace runs out: b(9) is mixed with A's side
/// missing, A's audio gone at its weight — one owned miss, never a fill.
#[test]
fn a_fade_with_a_playlists_side_missing_is_one_owned_miss() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(fade_300()));
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=8);
    assert_eq!(
        core.status().health.filled_owned,
        0,
        "both sides here: nothing missing"
    );
    core.offer(SRC_B, job(8, &fb, b(9), 0.2));
    core.release(b(9) + grace() + MS);
    let h = core.status().health;
    assert_eq!((h.filled, h.filled_owned), (0, 1));
    assert_eq!(core.status().transition.counters.side_fills, 1);
}

/// A fade INTO Blank: Blank never offers, so every mixed boundary has its
/// incoming side missing — black and silence by design, never owned; A's
/// side is here.
#[test]
fn a_fade_into_blank_misses_no_owned_side() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(fade_300()));
    let fa = frame(4, 2);
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
    }
    assert!(core.cut(PROGRAM_BLANK_ID, b(5) + 5 * MS));
    core.offer(SRC_A, job(4, &fa, b(6), 0.1));
    core.offer(SRC_A, job(4, &fa, b(7), 0.1));
    core.release(b(7) + MS);
    let status = core.status();
    assert_eq!(status.transition.counters.side_fills, 1, "b(7) was mixed");
    assert_eq!(status.health.filled_owned, 0);
}

/// A fade INTO Blank whose playlist sent nothing for b(8): Blank never
/// offers, so neither side is there and the boundary is filled. The fill
/// shows Blank, but the window's outgoing side is A's: its audio went
/// missing (#257 review), an owned fill.
#[test]
fn a_fade_into_blank_whose_playlist_missed_a_boundary_is_an_owned_fill() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(fade_300()));
    let fa = frame(4, 2);
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
    }
    assert!(core.cut(PROGRAM_BLANK_ID, b(5) + 5 * MS));
    core.offer(SRC_A, job(4, &fa, b(6), 0.1));
    core.offer(SRC_A, job(4, &fa, b(7), 0.1));
    core.release(b(8) + grace() + MS);
    let status = core.status();
    let h = status.health;
    assert_eq!((h.filled, h.filled_owned), (1, 1));
    assert_eq!(status.transition.counters.side_fills, 1, "b(7) was mixed");
}

/// `selected` (if any) on program; the sender's wall jumps from b(1) to
/// b(21), so b(2) to b(20) go in ONE resync.
fn one_resync(selected: Option<i64>) -> ProgramHealth {
    let mut core = ProgramCore::new();
    if let Some(pid) = selected {
        core.select_initial(pid);
    }
    core.release(b(1) + MS);
    core.release(b(21) + MS);
    core.status().health
}

#[test]
fn a_resync_is_owned_only_over_a_real_sources_boundaries() {
    let cases = [
        (Some(SRC_A), 1),
        (Some(PROGRAM_INPUT_ID), 1),
        (Some(PROGRAM_BLANK_ID), 0),
        (None, 0),
    ];
    for (selected, owned) in cases {
        let h = one_resync(selected);
        assert_eq!((h.resyncs, h.resyncs_owned), (1, owned), "{selected:?}");
    }
}
