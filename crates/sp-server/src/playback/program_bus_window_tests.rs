//! #257: `health.filled_owned` — a fill of a REAL source's boundary (a
//! playlist or the NDI input), and a fade's missing real side: the audio
//! that source should have had went missing, which the audio fault ledger
//! records as a loss; likewise `health.resyncs_owned`, a resync over a real
//! source's boundaries. Blank's fills and those of nothing selected are
//! silence by design and never count, and neither does a fill of a source
//! that has delivered nothing since it became the owner (a wait: the start's
//! restored selection, a cut to a source not at the bus yet) — for at most
//! 300 boundaries (10 s) after it became the owner. Reuses the #209 rig
//! (`program_bus_tests.rs`) and its #215 fade (`program_bus_tests_transition.rs`).
//! Wired via `#[cfg(test)] #[path = "program_bus_window_tests.rs"] mod tests;`
//! in `program_bus_window.rs`.

use std::ops::RangeInclusive;

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID};

use super::super::tests::{MS, SRC_A, SRC_B, b, frame, grace, job};
use super::super::tests_transition::{fade_300, fade_core, offer_both};
use super::super::{ProgramCore, ProgramHealth};
use super::real_source;
use crate::playback::program_transition::{SpecSource, TransitionSpec};

#[test]
fn a_real_source_is_a_playlist_or_the_ndi_input_never_blank_or_nothing() {
    assert!(real_source(Some(SRC_A)));
    assert!(real_source(Some(PROGRAM_INPUT_ID)));
    assert!(!real_source(Some(PROGRAM_BLANK_ID)));
    assert!(!real_source(None));
}

/// `selected` (if any) delivered b(1), then nothing: the sender's wall fills
/// b(2) once its grace ran out. With nothing selected nobody delivers: the
/// wall fills b(1).
fn one_fill(selected: Option<i64>) -> ProgramHealth {
    let mut core = ProgramCore::new();
    match selected {
        Some(pid) => {
            core.select_initial(pid);
            core.offer(pid, job(4, &frame(4, 2), b(1), 0.1));
            core.release(b(2) + grace() + MS);
        }
        None => core.release(b(1) + MS),
    }
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

/// `selected` (if any) on program, delivered b(1); the sender's wall jumps
/// to b(21), so b(2) to b(20) go in ONE resync. With nothing selected the
/// wall fills b(1) first.
fn one_resync(selected: Option<i64>) -> ProgramHealth {
    let mut core = ProgramCore::new();
    match selected {
        Some(pid) => {
            core.select_initial(pid);
            core.offer(pid, job(4, &frame(4, 2), b(1), 0.1));
        }
        None => core.release(b(1) + MS),
    }
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

/// The four program counters: fills, owned fills, resyncs, owned resyncs.
fn fills_and_resyncs(core: &ProgramCore) -> (u64, u64, u64, u64) {
    let h = core.status().health;
    (h.filled, h.filled_owned, h.resyncs, h.resyncs_owned)
}

/// #257: at a SongPlayer start the restored selection owns every boundary
/// before its pipeline delivered anything (SNV 12.10.2026, the deploy of
/// 26a5508d: the restore at 23:17:00.768, the Play at .830, and the ledger's
/// next tick wrote a `program` `filled` loss — after every deploy). Those
/// fills and that resync are a wait, never owned; once the source delivered,
/// a run it misses is its audio gone.
#[test]
fn the_restored_selection_owes_nothing_before_it_delivered() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.release(b(1) + MS);
    core.release(b(21) + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (2, 0, 1, 0),
        "a wait, never a loss"
    );
    core.offer(SRC_A, job(4, &frame(4, 2), b(22), 0.1));
    core.release(b(40) + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (2, 0, 2, 1),
        "delivered b(22), then missed"
    );
}

/// A cut to B, which has not delivered since the cut: its first missed
/// boundary is a wait (B's touch from BEFORE the cut does not count: it must
/// deliver since it became the owner). Once B delivered b(6), its miss of
/// b(7) is owned.
#[test]
fn a_source_cut_to_owes_its_misses_only_once_it_delivered_since_the_cut() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    core.touch(SRC_B, b(1));
    for k in 1..=3 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
    }
    assert!(core.cut(SRC_B, b(3) + 5 * MS));
    core.offer(SRC_A, job(4, &fa, b(4), 0.1)); // A's last: the cut is on b(5)
    core.release(b(5) + grace() + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (1, 0, 0, 0),
        "B has not delivered yet"
    );
    core.offer(SRC_B, job(8, &fb, b(6), 0.2));
    core.release(b(7) + grace() + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (2, 1, 0, 0),
        "B delivered b(6), then missed b(7)"
    );
}

/// The sender's wall reaches `b(k) + 1 ms` for each `k` of `ks`, in order.
fn release_through(core: &mut ProgramCore, ks: RangeInclusive<usize>) {
    for k in ks {
        core.release(b(k) + MS);
    }
}

/// Review: a source that never reaches the bus waits no longer than the
/// delivery grace, 300 boundaries (10 s) after it became the owner; the
/// restored selection owns from the program's first boundary. A never
/// offers: b(1) … b(300) are a wait, b(301) — 10 s after b(1) — is its audio
/// gone, or a hung pipeline would read "bez chyby" for as long as it hangs.
#[test]
fn the_restored_selection_owes_its_misses_once_the_delivery_grace_ran_out() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    release_through(&mut core, 1..=300);
    assert_eq!(fills_and_resyncs(&core), (300, 0, 0, 0), "inside the grace");
    core.release(b(301) + MS);
    assert_eq!(fills_and_resyncs(&core), (301, 1, 0, 0), "past the grace");
}

/// Review: a cut to a source that hangs (it never touches the bus): its
/// misses are a wait for 300 boundaries from the cut boundary b(5), then
/// owned — the program black and silent never reads "bez chyby" for long.
#[test]
fn a_hung_source_cut_to_owes_its_misses_once_the_delivery_grace_ran_out() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let fa = frame(4, 2);
    for k in 1..=3 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
    }
    assert!(core.cut(SRC_B, b(3) + 5 * MS));
    core.offer(SRC_A, job(4, &fa, b(4), 0.1)); // A's last: the cut is on b(5)
    release_through(&mut core, 5..=304);
    assert_eq!(
        fills_and_resyncs(&core),
        (300, 0, 0, 0),
        "b(5) … b(304): a wait"
    );
    core.release(b(305) + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (301, 1, 0, 0),
        "b(305), 300 boundaries after the cut"
    );
}

/// Review: the outgoing source of a running fade stalls (A sends nothing
/// after b(6)), and the operator cuts back to it (a Cut by then, on b(11):
/// B's newest stamp is b(9)). A delivered nothing since it became the owner
/// again: b(11) … b(310) are a wait, b(311) is owned.
#[test]
fn a_stalled_fade_source_cut_back_to_owes_its_misses_once_the_grace_ran_out() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6); // the fade A → B from b(7)
    for k in 7..=9 {
        core.offer(SRC_B, job(8, &fb, b(k), 0.2));
    }
    assert!(core.set_transition(TransitionSpec::cut(SpecSource::Setting)));
    assert!(core.cut(SRC_A, b(9) + 5 * MS));
    core.offer(SRC_B, job(8, &fb, b(10), 0.2)); // B's last boundary
    core.release(b(10) + grace() + MS); // the fade's b(7) … b(10), A's side missed
    let (filled, owned, resyncs, resyncs_owned) = fills_and_resyncs(&core);
    release_through(&mut core, 11..=310);
    assert_eq!(
        fills_and_resyncs(&core),
        (filled + 300, owned, resyncs, resyncs_owned),
        "b(11) … b(310): a wait"
    );
    core.release(b(311) + MS);
    assert_eq!(
        fills_and_resyncs(&core),
        (filled + 301, owned + 1, resyncs, resyncs_owned),
        "b(311), 300 boundaries after the cut back"
    );
}
