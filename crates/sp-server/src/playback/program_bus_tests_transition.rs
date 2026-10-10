//! #215 program transitions on the bus: a cut is a transition WINDOW. Each of
//! its boundaries waits for BOTH sources' pairs (the #209 reorder and fill
//! rules, per source), is queued as ONE `ProgramJob::Mix`, and a side that is
//! missed is mixed against the standby. A fade starts on the incoming
//! source's first live pair (the cue gate): an incoming side missing on the
//! cut boundary holds the outgoing source on program instead. `hold_for` keeps
//! the outgoing playlist playing through its window, and a Cut is the
//! zero-length window, the #209 cut unchanged. Reuses the #209 rig (`program_bus_tests.rs`): A's frames are
//! 4×2 NV12, B's 8×2, C's 6×2, the program's standby black 2×2.
//! Wired via `#[cfg(test)] #[path = "program_bus_tests_transition.rs"] mod tests_transition;`.

use super::tests::{
    MS, SRC_A, SRC_B, SRC_C, b, dims, drain, frame, grace, job, program, sends, shown_dims, stamps,
};
use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_transition::{
    ActiveWindow, SpecSource, TransitionCounters, TransitionKind, TransitionSpec, TransitionStatus,
    crossfade_gains,
};
use crate::playback::submit_handoff::SubmitJob;

/// The audio level of every block A (the outgoing source) offers.
pub(super) const LEVEL_A: f32 = 0.1;
/// The audio level of every block B (the incoming source) offers.
pub(super) const LEVEL_B: f32 = 0.2;

/// The 300 ms fade (9 slots at 30 fps) the rigs cut with.
pub(super) fn fade_300() -> TransitionSpec {
    TransitionSpec::fade(300, SpecSource::Setting)
}

/// A on program with the 300 ms fade in force.
pub(super) fn fade_core() -> ProgramCore {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(fade_300()), "Cut → Fade is a change");
    core
}

/// A and B offer every boundary of `ks`; after b(5) the operator cuts to B, so
/// the window is b(7)..=b(15) (end b(16), exclusive).
pub(super) fn offer_both(
    core: &mut ProgramCore,
    fa: &SharedFrame,
    fb: &SharedFrame,
    ks: std::ops::RangeInclusive<usize>,
) {
    for k in ks {
        core.offer(SRC_A, job(4, fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
    }
}

/// The fade from A to B running: b(1)..=b(8) are out (b(7) and b(8) mixed).
fn running_window() -> (ProgramCore, SharedFrame, SharedFrame) {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=8);
    assert_eq!(take_all(&mut core).len(), 8, "b(1)..=b(8) are out");
    (core, fa, fb)
}

/// A side's width, `-` when that side is missing (mixed as the standby).
fn width(side: Option<&SubmitJob>) -> String {
    side.map_or_else(|| "-".to_string(), |j| j.width.to_string())
}

/// Every queued program boundary as `(stamp, what)`: `src W` (a source's
/// own pair), `fill` (the program's standby pair) or `mix k/n F>T` (slot `k`
/// of an `n`-slot window, the outgoing and the incoming side's width).
pub(super) fn take_all(core: &mut ProgramCore) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    while let Some(job) = core.take() {
        let what = match &job {
            ProgramJob::Source(j) => format!("src {}", j.width),
            ProgramJob::Standby { .. } => "fill".to_string(),
            ProgramJob::Mix(m) => format!(
                "mix {}/{} {}>{}",
                m.slot,
                m.n_slots,
                width(m.from.as_ref()),
                width(m.to.as_ref())
            ),
        };
        out.push((job.stamp_100ns(), what));
    }
    out
}

/// `(b(k), what)` for every `k` of `ks`.
pub(super) fn run(
    ks: std::ops::RangeInclusive<usize>,
    what: impl Fn(usize) -> String,
) -> Vec<(i64, String)> {
    ks.map(|k| (b(k), what(k))).collect()
}

pub(super) fn one(k: usize, what: &str) -> Vec<(i64, String)> {
    vec![(b(k), what.to_string())]
}

#[test]
fn a_fade_mixes_exactly_its_n_boundaries_then_the_new_source_alone() {
    let mut core = fade_core();
    assert!(!core.set_transition(fade_300()), "the same spec again");
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=18 {
        let a = core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        let bo = core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        let want_a = if k <= 15 {
            OfferOutcome::Accepted
        } else {
            OfferOutcome::NotOwner
        };
        let want_b = if k >= 7 {
            OfferOutcome::Accepted
        } else {
            OfferOutcome::NotOwner
        };
        assert_eq!((a, bo), (want_a, want_b), "the offers of b({k})");
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
            assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
        }
        sent.extend(take_all(&mut core));
        let st = core.status();
        assert_eq!(
            st.transition.counters.transitions_done,
            u64::from(k >= 15),
            "the window is done exactly when its last boundary b(15) is served (after b({k}))"
        );
        assert_eq!(
            core.is_candidate(SRC_A),
            k < 15,
            "A contributes until b(15) is served (after b({k}))"
        );
        if k == 10 {
            assert_eq!(
                st.transition.active,
                Some(ActiveWindow {
                    from: Some(SRC_A),
                    to: SRC_B,
                    start_boundary_100ns: b(7),
                    n_slots: 9,
                    served_slots: 4,
                    progress: 44,
                })
            );
        }
    }
    let mut want = run(1..=6, |_| "src 4".to_string());
    want.extend(run(7..=15, |k| format!("mix {}/9 4>8", k - 7)));
    want.extend(run(16..=18, |_| "src 8".to_string()));
    assert_eq!(sent, want, "9 mixed boundaries, then B alone");
    let st = core.status();
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 1,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 0,
            cue_timeouts: 0,
        },
        "B was live on the cut boundary: no wait"
    );
    assert_eq!(st.transition.active, None, "the window is over");
    let h = st.health;
    assert_eq!(
        (h.forwarded, h.filled, h.late_dropped, h.cuts),
        (9, 0, 0, 1)
    );
    assert!(core.is_candidate(SRC_B));
}

#[test]
fn the_window_audio_is_the_equal_power_crossfade_and_never_dips_below_the_quieter_source() {
    let mut core = fade_core();
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut blocks = Vec::new();
    for k in 1..=18 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            core.cut(SRC_B, b(5) + 5 * MS);
        }
        while let Some(job) = core.take() {
            let stamp = out.submit(job);
            core.record_submitted(stamp);
            blocks.push((stamp, backend.last_audio_planar()));
        }
    }
    assert_eq!(
        backend.video_timecodes(),
        stamps(1..=18),
        "one picture per boundary"
    );
    assert_eq!(
        sends(&backend).len(),
        36,
        "exactly one audio block + one picture per boundary"
    );
    // A (4×2) and B (8×2) differ in size: every window boundary is a blend
    // (#215 addendum A; #223: both fitted into the rig's 2×2 canvas), never a
    // midpoint cut. `shown_dims` names a mixed boundary by B, its incoming side.
    assert_eq!(shown_dims(&out), dims(&[("4x2", 6), ("8x2", 12)]));
    assert_eq!(blocks.len(), 18);
    let total = 9 * 1600u64;
    for (k, (stamp, planar)) in (1..=18usize).zip(&blocks) {
        assert_eq!(*stamp, b(k));
        assert_eq!(planar.len(), 3200, "b({k}): one stereo 1600-sample block");
        let (left, right) = planar.split_at(1600);
        assert_eq!(left, right, "b({k}): both channels alike");
        for (i, &s) in left.iter().enumerate() {
            let want = match k {
                1..=6 => LEVEL_A,
                7..=15 => {
                    let (g_from, g_to) = crossfade_gains((k as u64 - 7) * 1600 + i as u64, total);
                    g_from * LEVEL_A + g_to * LEVEL_B
                }
                _ => LEVEL_B,
            };
            assert_eq!(s, want, "b({k}) sample {i}");
            assert!(
                s >= LEVEL_A,
                "b({k}) sample {i}: {s} dips below the quieter source"
            );
        }
    }
}

#[test]
fn a_window_boundary_waits_for_both_sides_and_a_repeated_side_is_late() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    assert_eq!(
        core.offer(SRC_B, job(8, &fb, b(7), LEVEL_B)),
        OfferOutcome::Accepted
    );
    assert_eq!(core.queued(), 0, "B's b(7) waits for A's b(7)");
    core.release(b(7) + MS);
    core.release(b(7) + grace() - 1);
    assert_eq!(
        core.queued(),
        0,
        "A is live and inside its grace: the sender waits too"
    );
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(7), LEVEL_A)),
        OfferOutcome::Accepted
    );
    assert_eq!(take_all(&mut core), one(7, "mix 0/9 4>8"));
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(7), LEVEL_A)),
        OfferOutcome::Late,
        "an outgoing pair for a served boundary"
    );
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(8), LEVEL_A)),
        OfferOutcome::Accepted
    );
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(8), LEVEL_A)),
        OfferOutcome::Late,
        "a duplicate of a waiting outgoing pair"
    );
    let st = core.status();
    assert_eq!(st.health.late_dropped, 2);
    assert_eq!(st.transition.counters.mixed_boundaries, 1);
}

#[test]
fn a_stalled_incoming_side_is_mixed_against_the_standby_after_its_grace() {
    let (mut core, fa, fb) = running_window();
    core.offer(SRC_A, job(4, &fa, b(9), LEVEL_A));
    core.offer(SRC_A, job(4, &fa, b(10), LEVEL_A));
    core.release(b(9) + grace() - 1);
    assert_eq!(core.queued(), 0, "B is still inside its grace");
    core.release(b(9) + grace());
    assert_eq!(take_all(&mut core), one(9, "mix 2/9 4>-"));
    core.release(b(10) + grace());
    assert_eq!(take_all(&mut core), one(10, "mix 3/9 4>-"));
    // B recovers: its late b(10) is dropped, b(11) mixes both sides again.
    assert_eq!(
        core.offer(SRC_B, job(8, &fb, b(10), LEVEL_B)),
        OfferOutcome::Late
    );
    core.offer(SRC_B, job(8, &fb, b(11), LEVEL_B));
    core.offer(SRC_A, job(4, &fa, b(11), LEVEL_A));
    assert_eq!(take_all(&mut core), one(11, "mix 4/9 4>8"));
    let st = core.status();
    let c = st.transition.counters;
    assert_eq!((c.mixed_boundaries, c.side_fills), (5, 2));
    assert_eq!(st.health.filled, 0, "never the program's standby pair");
}

#[test]
fn a_stalled_outgoing_side_is_mixed_against_the_standby_after_its_grace() {
    let (mut core, _, fb) = running_window();
    core.offer(SRC_B, job(8, &fb, b(9), LEVEL_B));
    core.offer(SRC_B, job(8, &fb, b(10), LEVEL_B));
    core.release(b(9) + grace() - 1);
    assert_eq!(core.queued(), 0, "A is still inside its grace");
    core.release(b(9) + grace());
    assert_eq!(take_all(&mut core), one(9, "mix 2/9 ->8"));
    let c = core.status().transition.counters;
    assert_eq!((c.mixed_boundaries, c.side_fills), (3, 1));
}

#[test]
fn an_incoming_side_missing_on_the_cut_boundary_holds_the_outgoing_source_until_it_delivers() {
    // #215 cue gate: the fade starts on the incoming source's first live
    // pair. Until B delivers one, A stays on program at full level (its own
    // pair) — never a mix against B's standby.
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    assert_eq!(
        core.hold_for(SRC_A),
        Some(Hold::Until(b(32))),
        "the fade may wait up to 15 boundaries: A is held through b(31)"
    );
    core.offer(SRC_A, job(4, &fa, b(7), LEVEL_A));
    core.offer(SRC_A, job(4, &fa, b(8), LEVEL_A));
    core.release(b(7) + grace());
    assert_eq!(
        take_all(&mut core),
        one(7, "src 4"),
        "B missed b(7): A holds it"
    );
    core.release(b(8) + grace());
    assert_eq!(take_all(&mut core), one(8, "src 4"));
    // B's first pair is b(9): the fade runs b(9)..=b(17).
    core.offer(SRC_B, job(8, &fb, b(9), LEVEL_B));
    assert_eq!(core.queued(), 0, "B's b(9) waits for A's");
    core.offer(SRC_A, job(4, &fa, b(9), LEVEL_A));
    assert_eq!(take_all(&mut core), one(9, "mix 0/9 4>8"));
    assert_eq!(
        core.hold_for(SRC_A),
        Some(Hold::Until(b(19))),
        "the fade is laid: A is held through its last boundary b(17), plus one slot"
    );
    let mut sent = Vec::new();
    offer_both(&mut core, &fa, &fb, 10..=18);
    sent.extend(take_all(&mut core));
    let mut want = run(10..=17, |k| format!("mix {}/9 4>8", k - 9));
    want.extend(one(18, "src 8"));
    assert_eq!(sent, want, "exactly 9 mixed boundaries, then B alone");
    let st = core.status();
    let c = st.transition.counters;
    assert_eq!(
        (c.transitions_done, c.mixed_boundaries, c.side_fills),
        (1, 9, 0)
    );
    assert_eq!(
        (st.health.forwarded, st.health.filled),
        (9, 0),
        "b(1)..=b(6) + the two held boundaries + b(18)"
    );
}

#[test]
fn a_side_already_past_its_boundary_is_missed_at_once_but_the_other_is_still_waited_for() {
    let (mut core, fa, fb) = running_window();
    // A's handoff coalesced b(9) away: it offers b(10). B's b(9) is still
    // coming, so nothing goes out yet.
    core.offer(SRC_A, job(4, &fa, b(10), LEVEL_A));
    assert_eq!(core.queued(), 0, "B's b(9) is still coming");
    core.offer(SRC_B, job(8, &fb, b(9), LEVEL_B));
    assert_eq!(
        take_all(&mut core),
        one(9, "mix 2/9 ->8"),
        "A is past b(9): mixed against its standby at once, well inside every grace"
    );
    core.offer(SRC_B, job(8, &fb, b(10), LEVEL_B));
    assert_eq!(take_all(&mut core), one(10, "mix 3/9 4>8"));
    assert_eq!(core.status().transition.counters.side_fills, 1);
}

#[test]
fn a_window_boundary_neither_side_delivers_is_filled_like_any_other() {
    // On the sender's wall: both sides stalled past the grace.
    let (mut core, _, _) = running_window();
    core.release(b(9) + grace());
    assert_eq!(take_all(&mut core), one(9, "fill"));
    let st = core.status();
    let c = st.transition.counters;
    assert_eq!(
        (st.health.filled, c.mixed_boundaries, c.side_fills),
        (1, 2, 0)
    );

    // On the offer path: both sources skipped b(9).
    let (mut core, fa, fb) = running_window();
    core.offer(SRC_A, job(4, &fa, b(10), LEVEL_A));
    assert_eq!(core.queued(), 0, "B may still deliver b(9)");
    core.offer(SRC_B, job(8, &fb, b(10), LEVEL_B));
    let mut want = one(9, "fill");
    want.extend(one(10, "mix 3/9 4>8"));
    assert_eq!(take_all(&mut core), want);
}

#[test]
fn a_fade_up_from_nothing_mixes_the_new_source_against_the_standby() {
    let mut core = ProgramCore::new();
    assert!(core.set_transition(fade_300()));
    let fb = frame(8, 2);
    for k in 1..=5 {
        assert_eq!(
            core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B)),
            OfferOutcome::NotOwner
        );
    }
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert_eq!(
        core.status()
            .transition
            .active
            .map(|w| (w.from, w.to, w.start_boundary_100ns)),
        Some((None, SRC_B, b(7)))
    );
    assert_eq!(
        core.hold_for(SRC_B),
        None,
        "the selected source is never held"
    );
    let mut sent = Vec::new();
    for k in 6..=17 {
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        sent.extend(take_all(&mut core));
    }
    let mut want = run(7..=15, |k| format!("mix {}/9 ->8", k - 7));
    want.extend(run(16..=17, |_| "src 8".to_string()));
    assert_eq!(sent, want);
    let c = core.status().transition.counters;
    assert_eq!(
        (c.mixed_boundaries, c.side_fills, c.transitions_done),
        (9, 0, 1),
        "nothing was on program: no side is missing"
    );
}

/// #245: a fade into Blank starts ON its cut boundary (no cue wait: Blank
/// never offers a live pair). Each of its 9 boundaries mixes A against the
/// standby, A is held playing through it, then the program carries the
/// standby pair alone.
#[test]
fn a_fade_into_blank_mixes_from_the_cut_boundary_then_fills() {
    let blank = sp_core::config::PROGRAM_BLANK_ID;
    let mut core = fade_core();
    let fa = frame(4, 2);
    let mut sent = Vec::new();
    for k in 1..=17 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        if k == 5 {
            assert!(core.cut(blank, b(5) + 5 * MS));
            assert!(core.hold_for(SRC_A).is_some(), "A plays through the fade");
        }
        core.release(b(k) + 5 * MS);
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=6, |_| "src 4".to_string());
    want.extend(run(7..=15, |k| format!("mix {}/9 4>-", k - 7)));
    want.extend(run(16..=17, |_| "fill".to_string()));
    assert_eq!(sent, want);
    let c = core.status().transition.counters;
    assert_eq!(
        (c.cue_timeouts, c.cue_wait_boundaries, c.transitions_done),
        (0, 0, 1),
        "no cue wait"
    );
}

#[test]
fn a_cut_is_the_zero_length_window_the_209_cut_unchanged() {
    assert_eq!(
        ProgramCore::new().status().transition,
        TransitionStatus {
            kind: TransitionKind::Cut,
            duration_ms: 0,
            n_slots: 0,
            source: SpecSource::Fallback,
            active: None,
            counters: TransitionCounters::default(),
        },
        "until the transition-settings task sets one, every cut is a hard cut"
    );
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(TransitionSpec::cut(SpecSource::Setting)));
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=12 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
            assert_eq!(core.status().transition.active, None, "no window to show");
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(8))),
                "A plays through its last boundary b(6), plus one slot"
            );
        }
        if k == 6 {
            assert_eq!(core.hold_for(SRC_A), None, "b(6) served: A may pause");
        }
        drain(&mut core, &mut out);
    }
    assert_eq!(backend.video_timecodes(), stamps(1..=12));
    assert_eq!(shown_dims(&out), dims(&[("4x2", 6), ("8x2", 6)]));
    let st = core.status();
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 1,
            ..TransitionCounters::default()
        },
        "a Cut never waits for a cue"
    );
    assert_eq!((st.health.forwarded, st.health.filled), (12, 0));
}

#[test]
fn hold_for_keeps_the_outgoing_source_until_one_slot_after_its_window() {
    assert_eq!(
        ProgramCore::new().hold_for(SRC_A),
        None,
        "no program source"
    );
    let mut core = fade_core();
    assert_eq!(
        core.hold_for(SRC_A),
        None,
        "#221 L4b: the selected source is never held (the authority never takes it off)"
    );
    assert_eq!(core.hold_for(SRC_B), None);
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    // Drained in two batches: the program queue holds 10 boundaries.
    offer_both(&mut core, &fa, &fb, 1..=8);
    assert_eq!(take_all(&mut core).len(), 8);
    offer_both(&mut core, &fa, &fb, 9..=14);
    assert_eq!(take_all(&mut core).len(), 6);
    assert_eq!(core.status().health.coalesced, 0);
    assert_eq!(
        core.hold_for(SRC_A),
        Some(Hold::Until(b(17))),
        "the window ends at b(16) (exclusive), plus one slot"
    );
    assert_eq!(core.hold_for(SRC_B), None);
    assert_eq!(core.hold_for(SRC_C), None);
    offer_both(&mut core, &fa, &fb, 15..=15);
    assert_eq!(
        core.hold_for(SRC_A),
        None,
        "b(15) served: the window is over"
    );

    // The same through the thread-safe bus.
    let bus = ProgramBus::new();
    bus.select_initial(SRC_A, None);
    assert_eq!(bus.hold_for(SRC_A), None);
    assert!(bus.set_transition(fade_300()));
    assert!(!bus.set_transition(fade_300()), "the same spec again");
    let st = bus.cut(SRC_B, b(5), None);
    assert_eq!(st.cut_boundary_100ns, Some(b(7)));
    assert_eq!(
        (
            st.transition.kind,
            st.transition.n_slots,
            st.transition.source
        ),
        (TransitionKind::Fade, 9, SpecSource::Setting)
    );
    assert_eq!(
        bus.hold_for(SRC_A),
        Some(Hold::Until(b(32))),
        "B has sent nothing yet: the fade may wait up to 15 boundaries for its \
         first live pair, so the window may end as late as b(31)"
    );
}

#[test]
fn a_second_cut_inside_a_window_truncates_it_and_opens_the_next_window() {
    let mut core = fade_core();
    let (fa, fb, fc) = (frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    for k in 1..=22 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 9 {
            assert!(core.cut(SRC_C, b(9) + 5 * MS));
            assert_eq!(core.status().cut_boundary_100ns, Some(b(11)));
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(12))),
                "the first window now ends at b(11)"
            );
            assert_eq!(
                core.hold_for(SRC_B),
                Some(Hold::Until(b(36))),
                "C has sent no pair for b(11) yet: B's fade-out may wait up to 15 \
                 boundaries, so it may end as late as b(35)"
            );
            assert_eq!(core.hold_for(SRC_C), None);
            assert_eq!(core.status().transition.counters.transitions_done, 0);
        }
        if k == 10 {
            let st = core.status();
            assert_eq!(
                st.transition.counters.transitions_done, 1,
                "the truncated first window is done, the second is still to come"
            );
            assert_eq!(
                st.transition.active,
                Some(ActiveWindow {
                    from: Some(SRC_B),
                    to: SRC_C,
                    start_boundary_100ns: b(11),
                    n_slots: 9,
                    served_slots: 0,
                    progress: 0,
                })
            );
            assert!(!core.is_candidate(SRC_A));
            assert_eq!(core.hold_for(SRC_A), None);
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=6, |_| "src 4".to_string());
    want.extend(run(7..=10, |k| format!("mix {}/9 4>8", k - 7)));
    want.extend(run(11..=19, |k| format!("mix {}/9 8>6", k - 11)));
    want.extend(run(20..=22, |_| "src 6".to_string()));
    assert_eq!(sent, want);
    assert_eq!(
        core.status().transition.counters,
        TransitionCounters {
            transitions_done: 2,
            mixed_boundaries: 13,
            side_fills: 0,
            cue_wait_boundaries: 0,
            cue_timeouts: 0,
        },
        "B and then C were live on their cut boundaries"
    );
}

#[test]
fn a_cut_during_a_window_lands_after_the_outgoing_sources_newest_stamp() {
    let (mut core, fa, fb) = running_window();
    offer_both(&mut core, &fa, &fb, 9..=9);
    // A's submit thread already reached b(14); B and the program are at b(9).
    assert!(
        core.touch(SRC_A, b(14)),
        "A still contributes to the window"
    );
    assert!(core.cut(SRC_C, b(9) + 5 * MS));
    assert_eq!(
        core.status().cut_boundary_100ns,
        Some(b(16)),
        "after A's b(14): the next boundary + 1 slot"
    );
}

#[test]
fn cutting_back_inside_the_same_slot_cancels_the_window() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
    }
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert!(core.status().transition.active.is_some());
    assert!(core.cut(SRC_A, b(5) + 6 * MS)); // same slot: back to A
    let st = core.status();
    assert_eq!(st.transition.active, None, "no window is left");
    assert_eq!(st.health.cuts, 2);
    assert_eq!(core.hold_for(SRC_A), None);
    assert_eq!(core.hold_for(SRC_B), None);
    assert!(!core.is_candidate(SRC_B));
    let mut sent = Vec::new();
    for k in 6..=11 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        sent.extend(take_all(&mut core));
    }
    assert_eq!(sent, run(1..=11, |_| "src 4".to_string()));
    assert_eq!(
        core.status().transition.counters,
        TransitionCounters::default()
    );
}

#[test]
fn a_second_cut_on_the_same_boundary_replaces_the_windows_target() {
    let mut core = fade_core();
    let fa = frame(4, 2);
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
    }
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert!(core.cut(SRC_C, b(5) + 6 * MS)); // also b(7): replaces B
    assert_eq!(
        core.status()
            .transition
            .active
            .map(|w| (w.from, w.to, w.start_boundary_100ns)),
        Some((Some(SRC_A), SRC_C, b(7)))
    );
    assert!(!core.is_candidate(SRC_B), "the replaced target never plays");
    assert_eq!(
        core.hold_for(SRC_A),
        Some(Hold::Until(b(32))),
        "C has sent nothing yet: the window may end as late as b(31)"
    );
}

/// A on program with a 1000 ms fade (30 slots, b(7)..=b(36)); b(1)..=b(6) out.
fn long_fade() -> (ProgramCore, SharedFrame, SharedFrame) {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(TransitionSpec::fade(1000, SpecSource::Setting)));
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    (core, fa, fb)
}

#[test]
fn the_outgoing_sides_reorder_buffer_forces_one_boundary_past_its_bound() {
    let (mut core, fa, _) = long_fade();
    for k in 7..7 + PROGRAM_PENDING_BOUND {
        assert_eq!(
            core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A)),
            OfferOutcome::Accepted
        );
    }
    assert_eq!(
        core.queued(),
        0,
        "{PROGRAM_PENDING_BOUND} waiting outgoing pairs still wait for B"
    );
    core.offer(SRC_A, job(4, &fa, b(7 + PROGRAM_PENDING_BOUND), LEVEL_A));
    assert_eq!(
        take_all(&mut core),
        one(7, "src 4"),
        "one more forces b(7): B sent no live pair for it, so the fade has not \
         started and A stays on program at full level (#215 cue gate)"
    );
    assert_eq!(core.status().transition.counters.side_fills, 0);
}

#[test]
fn the_incoming_sides_reorder_buffer_forces_one_mix_past_its_bound() {
    let (mut core, _, fb) = long_fade();
    for k in 7..7 + PROGRAM_PENDING_BOUND {
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
    }
    assert_eq!(core.queued(), 0, "B's pairs wait for A's");
    core.offer(SRC_B, job(8, &fb, b(7 + PROGRAM_PENDING_BOUND), LEVEL_B));
    assert_eq!(take_all(&mut core), one(7, "mix 0/30 ->8"));
    assert_eq!(core.status().transition.counters.side_fills, 1);
}

#[test]
fn an_owner_that_never_offered_is_missed_exactly_when_its_boundary_is_reached() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(!core.fill_due(b(2), b(2) - 1));
    assert!(core.fill_due(b(2), b(2)));
}
