//! #215 addendum B2: the cue gate on the bus. A fade does not start on the
//! cut boundary: it waits for the incoming source's first LIVE pair
//! (`SubmitJob::live` — a decoder frame, never a paced-output fill, a pre-roll
//! black or a paused frozen picture), at most `CUE_WAIT_MAX_SLOTS` (15)
//! boundaries. Meanwhile each boundary is HELD: the outgoing source's own pair
//! at full level (the program's standby pair if it missed), the incoming
//! side's pair dropped. A Cut never waits. Reuses the #209 rig
//! (`program_bus_tests.rs`: A 4×2, B 8×2, C 6×2) and the #215 helpers
//! (`program_bus_tests_transition.rs`: `take_all` renders each job as
//! `src W` / `fill` / `mix k/n F>T`).
//! Wired via `#[cfg(test)] #[path = "program_bus_tests_cue.rs"] mod tests_cue;`.

use super::tests::{
    MS, SRC_A, SRC_B, SRC_C, b, dims, drain, frame, grace, job, program, shown_dims,
};
use super::tests_transition::{LEVEL_A, LEVEL_B, fade_core, offer_both, one, run, take_all};
use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_transition::{ActiveWindow, TransitionCounters, TransitionSpec};
use crate::playback::submit_handoff::SubmitJob;

/// A source's pair that is NOT live (a paced-output fill, the pre-roll black,
/// a paused frozen picture): the cue gate never opens on it.
fn standby(w: u32, video: &SharedFrame, stamp: i64, level: f32) -> SubmitJob {
    SubmitJob {
        live: false,
        ..job(w, video, stamp, level)
    }
}

#[test]
fn a_fade_waits_for_the_incoming_sources_first_live_pair_then_mixes_exactly_n_boundaries() {
    // B (the incoming playlist) sends standby pairs b(1)..=b(10) — its new
    // song is still opening — and its first live pair on b(11). The cut
    // lands on b(7): A stays on program at full level for b(7)..=b(10), the
    // fade runs b(11)..=b(19), then B alone.
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=24 {
        // B first: its pair for a boundary must wait for A's.
        let pair = if k >= 11 {
            job(8, &fb, b(k), LEVEL_B)
        } else {
            standby(8, &fb, b(k), LEVEL_B)
        };
        core.offer(SRC_B, pair);
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
            assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
        }
        if k == 9 {
            assert_eq!(
                core.status().transition.active,
                Some(ActiveWindow {
                    from: Some(SRC_A),
                    to: SRC_B,
                    start_boundary_100ns: b(7),
                    n_slots: 9,
                    served_slots: 0,
                    progress: 0,
                }),
                "waiting: no progress yet"
            );
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(32))),
                "A is held through the wait's worst case: 15 + 9 slots after b(7)"
            );
        }
        if k == 13 {
            assert_eq!(
                core.status().transition.active,
                Some(ActiveWindow {
                    from: Some(SRC_A),
                    to: SRC_B,
                    start_boundary_100ns: b(11),
                    n_slots: 9,
                    served_slots: 3,
                    progress: 33,
                })
            );
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(21))),
                "the fade is laid: A is held through b(19), plus one slot"
            );
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=10, |_| "src 4".to_string());
    want.extend(run(11..=19, |k| format!("mix {}/9 4>8", k - 11)));
    want.extend(run(20..=24, |_| "src 8".to_string()));
    assert_eq!(
        sent, want,
        "A held on b(7)..=b(10), 9 mixed boundaries from B's first live pair"
    );
    let st = core.status();
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 1,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 4,
            cue_timeouts: 0,
        }
    );
    let h = st.health;
    assert_eq!(
        (h.forwarded, h.filled, h.late_dropped),
        (15, 0, 0),
        "b(1)..=b(6), the 4 held boundaries and b(20)..=b(24)"
    );
}

#[test]
fn a_held_boundary_is_the_outgoing_sources_own_pair_at_full_level() {
    let mut core = fade_core();
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=10 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        drain(&mut core, &mut out);
        let planar = backend.last_audio_planar();
        assert_eq!(planar.len(), 3200, "b({k}): one stereo block");
        assert!(
            planar.iter().all(|&s| s == LEVEL_A),
            "b({k}): A at full level, nothing of B"
        );
    }
    assert_eq!(shown_dims(&out), dims(&[("4x2", 10)]));
}

#[test]
fn a_fade_whose_incoming_source_sends_no_live_pair_starts_after_fifteen_boundaries() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=33 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=21, |_| "src 4".to_string());
    want.extend(run(22..=30, |k| format!("mix {}/9 4>8", k - 22)));
    want.extend(run(31..=33, |_| "src 8".to_string()));
    assert_eq!(
        sent, want,
        "A held on the 15 boundaries b(7)..=b(21); the fade starts anyway on b(22)"
    );
    let st = core.status();
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 1,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 15,
            cue_timeouts: 1,
        }
    );
    assert_eq!(st.health.coalesced, 0);
}

#[test]
fn a_cut_stays_immediate_whatever_the_incoming_side_sends() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(TransitionSpec::cut(SpecSource::Setting)));
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=12 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(8))),
                "no wait: A plays through b(6), plus one slot"
            );
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=6, |_| "src 4".to_string());
    want.extend(run(7..=12, |_| "src 8".to_string()));
    assert_eq!(sent, want, "B from the cut boundary, live or not");
    assert_eq!(
        core.status().transition.counters,
        TransitionCounters {
            transitions_done: 1,
            ..TransitionCounters::default()
        }
    );
}

#[test]
fn an_outgoing_side_missing_while_the_cue_waits_is_the_programs_standby_pair() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    core.offer(SRC_B, standby(8, &fb, b(7), LEVEL_B));
    core.release(b(7) + grace());
    assert_eq!(
        take_all(&mut core),
        one(7, "fill"),
        "A missed b(7) and B is not live: the program's standby pair"
    );
    assert_eq!(core.status().health.filled, 1);
    assert!(
        core.pending.is_empty(),
        "B's standby pair for the held b(7) is dropped, never left waiting"
    );
    core.offer(SRC_A, job(4, &fa, b(8), LEVEL_A));
    assert_eq!(core.queued(), 0, "A's b(8) waits for B's");
    core.offer(SRC_B, job(8, &fb, b(8), LEVEL_B));
    assert_eq!(take_all(&mut core), one(8, "mix 0/9 4>8"));
    let c = core.status().transition.counters;
    assert_eq!((c.cue_wait_boundaries, c.cue_timeouts), (1, 0));
}

#[test]
fn both_sides_stalled_while_the_cue_waits_resync_like_any_boundary() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    // Nobody sends b(7)..; the sender's wall is 12 slots past b(7).
    core.release(b(19));
    assert!(
        take_all(&mut core).is_empty(),
        "no burst of standby pairs: the program resyncs"
    );
    let h = core.status().health;
    assert_eq!((h.resyncs, h.filled), (1, 0));
}

#[test]
fn a_second_cut_while_the_cue_waits_freezes_it_and_fades_out_of_the_source_on_air() {
    // A → B, but B sends only standby pairs; before it is live the operator
    // cuts to C. A never left program, so the new fade goes from A to C, and
    // B is never shown.
    let mut core = fade_core();
    let (fa, fb, fc) = (frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    for k in 1..=21 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_C, b(8) + 5 * MS));
            let st = core.status();
            assert_eq!(st.cut_boundary_100ns, Some(b(10)));
            assert_eq!(
                st.transition
                    .active
                    .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                Some((Some(SRC_A), SRC_C, b(10))),
                "the frozen A → B window is not shown; the next fades out of A"
            );
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(35))),
                "A is held through the new fade's worst case (b(10) + 15 + 9 slots)"
            );
            assert_eq!(core.hold_for(SRC_B), None, "B never went on program");
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=9, |_| "src 4".to_string());
    want.extend(run(10..=18, |k| format!("mix {}/9 4>6", k - 10)));
    want.extend(run(19..=21, |_| "src 6".to_string()));
    assert_eq!(sent, want);
    let st = core.status();
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 2,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 0,
            cue_timeouts: 0,
        }
    );
    assert_eq!(st.health.forwarded, 12, "b(1)..=b(9) and b(19)..=b(21)");
}

#[test]
fn cutting_back_to_the_source_a_waiting_cue_kept_on_air_needs_no_window() {
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=14 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_A, b(8) + 5 * MS), "back to A");
            let st = core.status();
            assert_eq!(
                (st.source, st.cut_boundary_100ns),
                (Some(SRC_A), Some(b(10)))
            );
            assert_eq!(st.transition.active, None, "no fade to show");
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(11))),
                "A keeps its held boundaries through b(9), then owns b(10) on"
            );
            assert_eq!(core.hold_for(SRC_B), None);
        }
        sent.extend(take_all(&mut core));
    }
    assert_eq!(sent, run(1..=14, |_| "src 4".to_string()), "A throughout");
    assert!(!core.is_candidate(SRC_B));
    assert_eq!(
        core.status().transition.counters,
        TransitionCounters {
            transitions_done: 1,
            ..TransitionCounters::default()
        },
        "the frozen window is done; nothing was mixed"
    );
}

#[test]
fn the_outgoing_pairs_past_the_fade_are_dropped_when_the_cue_opens() {
    // While the cue waits, A's pairs are taken up to the wait's worst case.
    // B's first live pair on b(7) lays the fade on b(7)..=b(15): A's pairs
    // for b(16).. never mix and must not wait forever.
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    offer_both(&mut core, &fa, &fb, 1..=6);
    assert_eq!(take_all(&mut core).len(), 6);
    for k in 7..=22 {
        assert_eq!(
            core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A)),
            OfferOutcome::Accepted
        );
    }
    assert_eq!(core.queued(), 0, "B has sent nothing yet");
    assert_eq!(core.from_pending.len(), 16);
    core.offer(SRC_B, job(8, &fb, b(7), LEVEL_B));
    assert_eq!(take_all(&mut core), one(7, "mix 0/9 4>8"));
    assert_eq!(
        core.from_pending.keys().copied().collect::<Vec<_>>(),
        (8..=15).map(b).collect::<Vec<_>>(),
        "A's b(16)..=b(22) are dropped"
    );
    let mut sent = Vec::new();
    for k in 8..=16 {
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        sent.extend(take_all(&mut core));
    }
    let mut want = run(8..=15, |k| format!("mix {}/9 4>8", k - 7));
    want.extend(one(16, "src 8"));
    assert_eq!(sent, want);
    assert!(core.from_pending.is_empty());
}

#[test]
fn a_frozen_window_never_opens_even_when_its_incoming_source_goes_live() {
    // Review round 1: the cut to C froze the A → B fade; B's first live pair
    // then lands on b(9), still inside the frozen window. It must stay held
    // (A at full level) — B was cut away before it ever went on program.
    let mut core = fade_core();
    let (fa, fb, fc) = (frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    for k in 1..=21 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        let pair_b = if k >= 9 {
            job(8, &fb, b(k), LEVEL_B)
        } else {
            standby(8, &fb, b(k), LEVEL_B)
        };
        core.offer(SRC_B, pair_b);
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_C, b(8) + 5 * MS));
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=9, |_| "src 4".to_string());
    want.extend(run(10..=18, |k| format!("mix {}/9 4>6", k - 10)));
    want.extend(run(19..=21, |_| "src 6".to_string()));
    assert_eq!(sent, want, "b(9) is held, never a mix into B");
    assert_eq!(
        core.status().transition.counters,
        TransitionCounters {
            transitions_done: 2,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 0,
            cue_timeouts: 0,
        }
    );
}

#[test]
fn a_same_slot_recut_after_a_freeze_still_fades_out_of_the_source_on_air() {
    // Review round 1: A → B (never live), then C on b(10) — the A → B fade is
    // frozen and A → C waits — then, in the same slot, D replaces C. A is
    // still the source on program, so the fade must go from A to D.
    const SRC_D: i64 = 44;
    let mut core = fade_core();
    let (fa, fb, fc, fd) = (frame(4, 2), frame(8, 2), frame(6, 2), frame(10, 2));
    let mut sent = Vec::new();
    for k in 1..=21 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        core.offer(SRC_D, job(10, &fd, b(k), 0.4));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_C, b(8) + 5 * MS));
            assert!(
                core.cut(SRC_D, b(8) + 6 * MS),
                "the same slot: D replaces C"
            );
            assert_eq!(core.status().cut_boundary_100ns, Some(b(10)));
            assert_eq!(
                core.status()
                    .transition
                    .active
                    .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                Some((Some(SRC_A), SRC_D, b(10))),
                "the fade goes out of A, the source on program"
            );
            assert_eq!(core.hold_for(SRC_B), None, "B never went on program");
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=9, |_| "src 4".to_string());
    want.extend(run(10..=18, |k| format!("mix {}/9 4>10", k - 10)));
    want.extend(run(19..=21, |_| "src 10".to_string()));
    assert_eq!(sent, want);
    let st = core.status();
    assert_eq!(st.health.cuts, 3);
    assert_eq!(
        st.transition.counters,
        TransitionCounters {
            transitions_done: 2,
            mixed_boundaries: 9,
            side_fills: 0,
            cue_wait_boundaries: 0,
            cue_timeouts: 0,
        },
        "the frozen A → B window and the A → D fade are done; the replaced \
         A → C window never counted"
    );
}

#[test]
fn a_cut_back_then_a_same_slot_recut_still_fades_out_of_the_source_on_air() {
    // Review round 3: A → B (never live), back to A on b(10) — the A → B fade
    // is frozen and A takes b(10) on — then C in the same slot. A is on
    // program right up to b(10), so the fade goes from A to C.
    let mut core = fade_core();
    let (fa, fb, fc) = (frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    for k in 1..=21 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_A, b(8) + 5 * MS), "back to A");
            assert!(core.cut(SRC_C, b(8) + 6 * MS), "then C, same slot");
            assert_eq!(
                core.status()
                    .transition
                    .active
                    .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                Some((Some(SRC_A), SRC_C, b(10)))
            );
            assert_eq!(core.hold_for(SRC_B), None, "B never went on program");
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=9, |_| "src 4".to_string());
    want.extend(run(10..=18, |k| format!("mix {}/9 4>6", k - 10)));
    want.extend(run(19..=21, |_| "src 6".to_string()));
    assert_eq!(sent, want);
}

#[test]
fn a_cut_back_then_back_again_in_the_same_slot_waits_for_the_first_live_pair() {
    // Review round 3: A → B, back to A on b(10), then B again in the same
    // slot. A is still on program, so this is a new A → B fade that waits for
    // B's first live pair (b(12)) — never a hard cut to B's standby pairs.
    let mut core = fade_core();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    let mut sent = Vec::new();
    for k in 1..=23 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        let pair_b = if k >= 12 {
            job(8, &fb, b(k), LEVEL_B)
        } else {
            standby(8, &fb, b(k), LEVEL_B)
        };
        core.offer(SRC_B, pair_b);
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        if k == 8 {
            assert!(core.cut(SRC_A, b(8) + 5 * MS), "back to A");
            assert!(core.cut(SRC_B, b(8) + 6 * MS), "B again, same slot");
            assert_eq!(core.status().cut_boundary_100ns, Some(b(10)));
            assert_eq!(
                core.hold_for(SRC_A),
                Some(Hold::Until(b(35))),
                "A is held through the new fade's worst case"
            );
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=11, |_| "src 4".to_string());
    want.extend(run(12..=20, |k| format!("mix {}/9 4>8", k - 12)));
    want.extend(run(21..=23, |_| "src 8".to_string()));
    assert_eq!(
        sent, want,
        "A held on b(7)..=b(11), the fade from B's first live pair"
    );
    let c = core.status().transition.counters;
    assert_eq!(
        (
            c.transitions_done,
            c.mixed_boundaries,
            c.cue_wait_boundaries,
            c.cue_timeouts
        ),
        (2, 9, 2, 0),
        "the frozen A → B window and the new A → B fade, which waited b(10), b(11)"
    );
}

#[test]
fn a_later_cut_can_land_before_a_frozen_windows_end_and_still_fades_out_of_the_source_on_air() {
    // Review round 4: cut boundaries are not monotone. X → A fades b(7)..b(15)
    // while X's submit thread runs two slots ahead, so the cut to B lands on
    // b(16) and the cut back to A (while X is still a window's source) on
    // b(18): the waiting A → B window is frozen to b(18). Once b(15) is
    // served X is no longer involved, and the cut to C lands on b(17) —
    // BEFORE that frozen window's end. A is on program there, so the fade
    // must go from A to C.
    const SRC_X: i64 = 55;
    let mut core = ProgramCore::new();
    core.select_initial(SRC_X);
    assert!(core.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
    let (fx, fa, fb, fc) = (frame(10, 2), frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    for k in 1..=27 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        // X: one pair per boundary, then from b(6) on two slots ahead.
        let x_stamps: Vec<usize> = match k {
            1..=5 => vec![k],
            6 => vec![6, 7, 8],
            _ => vec![k + 2],
        };
        for kx in x_stamps {
            core.offer(SRC_X, job(10, &fx, b(kx), 0.05));
        }
        core.offer(SRC_B, standby(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        match k {
            5 => assert!(core.cut(SRC_A, b(5) + 5 * MS)),
            12 => {
                assert!(core.cut(SRC_B, b(12) + 5 * MS));
                assert_eq!(core.status().cut_boundary_100ns, Some(b(16)));
            }
            14 => {
                assert!(core.cut(SRC_A, b(14) + 5 * MS), "back to A");
                assert_eq!(core.status().cut_boundary_100ns, Some(b(18)));
            }
            15 => {
                assert!(core.cut(SRC_C, b(15) + 5 * MS));
                let st = core.status();
                assert_eq!(st.cut_boundary_100ns, Some(b(17)), "before b(18)");
                assert_eq!(
                    st.transition
                        .active
                        .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                    Some((Some(SRC_A), SRC_C, b(17)))
                );
                assert_eq!(core.hold_for(SRC_B), None, "B never went on program");
            }
            _ => {}
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=6, |_| "src 10".to_string());
    want.extend(run(7..=15, |k| format!("mix {}/9 10>4", k - 7)));
    want.extend(one(16, "src 4"));
    want.extend(run(17..=25, |k| format!("mix {}/9 4>6", k - 17)));
    want.extend(run(26..=27, |_| "src 6".to_string()));
    assert_eq!(sent, want, "A held on b(16), then the A → C fade");
    let c = core.status().transition.counters;
    assert_eq!(
        (c.transitions_done, c.mixed_boundaries, c.side_fills),
        (3, 18, 0),
        "X → A, the frozen A → B window, A → C"
    );
}

#[test]
fn a_waiting_window_cut_after_the_new_boundary_does_not_decide_the_source_on_air() {
    // Review round 4, the waiting half: X → A fades b(7)..b(15) while X's
    // submit thread runs four slots ahead. A Cut to B lands on b(17), then a
    // fade to C on b(18) (its cue waits on B's pairs). Once b(15) is served X
    // is out, and the cut to D lands on b(17): it replaces both. On program
    // before b(17) is A — never B, whose Cut this cut replaces — so the fade
    // goes from A to D and B is not held.
    const SRC_X: i64 = 55;
    const SRC_D: i64 = 44;
    let mut core = ProgramCore::new();
    core.select_initial(SRC_X);
    assert!(core.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
    let (fx, fa, fb, fc, fd) = (
        frame(10, 2),
        frame(4, 2),
        frame(8, 2),
        frame(6, 2),
        frame(12, 2),
    );
    let mut sent = Vec::new();
    for k in 1..=27 {
        core.offer(SRC_A, job(4, &fa, b(k), LEVEL_A));
        // X: one pair per boundary, then from b(6) on four slots ahead.
        let x_stamps: Vec<usize> = match k {
            1..=5 => vec![k],
            6 => vec![6, 7, 8, 9, 10],
            _ => vec![k + 4],
        };
        for kx in x_stamps {
            core.offer(SRC_X, job(10, &fx, b(kx), 0.05));
        }
        core.offer(SRC_B, job(8, &fb, b(k), LEVEL_B));
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        core.offer(SRC_D, job(12, &fd, b(k), 0.4));
        match k {
            5 => assert!(core.cut(SRC_A, b(5) + 5 * MS)),
            11 => {
                assert!(core.set_transition(TransitionSpec::cut(SpecSource::Setting)));
                assert!(core.cut(SRC_B, b(11) + 5 * MS));
                assert_eq!(core.status().cut_boundary_100ns, Some(b(17)));
            }
            12 => {
                assert!(core.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
                assert!(core.cut(SRC_C, b(12) + 5 * MS));
                assert_eq!(core.status().cut_boundary_100ns, Some(b(18)));
                assert_eq!(
                    core.hold_for(SRC_B),
                    Some(Hold::Until(b(43))),
                    "the fade to C waits: B held to b(18) + 15 + 9, plus one slot"
                );
            }
            15 => {
                assert!(core.cut(SRC_D, b(15) + 5 * MS));
                let st = core.status();
                assert_eq!(st.cut_boundary_100ns, Some(b(17)), "on B's own Cut");
                assert_eq!(
                    st.transition
                        .active
                        .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                    Some((Some(SRC_A), SRC_D, b(17)))
                );
                assert_eq!(core.hold_for(SRC_B), None, "B never went on program");
            }
            _ => {}
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=6, |_| "src 10".to_string());
    want.extend(run(7..=15, |k| format!("mix {}/9 10>4", k - 7)));
    want.extend(one(16, "src 4"));
    want.extend(run(17..=25, |k| format!("mix {}/9 4>12", k - 17)));
    want.extend(run(26..=27, |_| "src 12".to_string()));
    assert_eq!(sent, want, "A alone on b(16), then the A → D fade");
    let c = core.status().transition.counters;
    assert_eq!(
        (c.transitions_done, c.mixed_boundaries, c.side_fills),
        (2, 18, 0),
        "X → A and A → D; the replaced Cut and fade never count"
    );
}

#[test]
fn a_waiting_window_that_ends_before_a_later_cut_still_opens_on_its_live_pair() {
    // Review round 5: a later cut freezes only a waiting window whose span
    // reaches it — the rule `on_air` decides the source on air by. A 1-slot
    // fade A → B cut on b(7) waits for B (standby pairs until its first live
    // pair on b(22), the cue's deadline); its latest end is b(23). A runs one
    // slot ahead, so the cut to C at k = 21 lands on b(24), after that end.
    // The A → B fade must still open on b(22) (frozen, it would hard-cut A → B
    // on b(23)), and the fade to C goes out of B.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.set_transition(TransitionSpec::fade(30, SpecSource::Setting)));
    assert_eq!(core.status().transition.n_slots, 1, "30 ms = one slot");
    let (fa, fb, fc) = (frame(4, 2), frame(8, 2), frame(6, 2));
    let mut sent = Vec::new();
    let mut offered_a = 0;
    for k in 1..=27 {
        // A: one pair per boundary, then from b(6) on one slot ahead.
        let upto = if k <= 5 { k } else { k + 1 };
        while offered_a < upto {
            offered_a += 1;
            core.offer(SRC_A, job(4, &fa, b(offered_a), LEVEL_A));
        }
        let pair = if k >= 22 {
            job(8, &fb, b(k), LEVEL_B)
        } else {
            standby(8, &fb, b(k), LEVEL_B)
        };
        core.offer(SRC_B, pair);
        core.offer(SRC_C, job(6, &fc, b(k), 0.3));
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
            assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
        }
        if k == 21 {
            assert!(core.cut(SRC_C, b(21) + 5 * MS));
            let st = core.status();
            assert_eq!(
                st.cut_boundary_100ns,
                Some(b(24)),
                "after the A → B window's latest end, b(23)"
            );
            assert_eq!(
                st.transition
                    .active
                    .map(|w| (w.from, w.to, w.start_boundary_100ns)),
                Some((Some(SRC_A), SRC_B, b(7))),
                "the A → B fade still waits for its cue"
            );
        }
        sent.extend(take_all(&mut core));
    }
    let mut want = run(1..=21, |_| "src 4".to_string());
    want.extend(one(22, "mix 0/1 4>8"));
    want.extend(one(23, "src 8"));
    want.extend(one(24, "mix 0/1 8>6"));
    want.extend(run(25..=27, |_| "src 6".to_string()));
    assert_eq!(sent, want, "A → B on B's first live pair, then B → C");
    let c = core.status().transition.counters;
    assert_eq!(
        (
            c.transitions_done,
            c.mixed_boundaries,
            c.side_fills,
            c.cue_wait_boundaries,
            c.cue_timeouts
        ),
        (2, 2, 0, 0, 0),
        "both fades mixed their slot; B → C opened live on its cut"
    );
}
