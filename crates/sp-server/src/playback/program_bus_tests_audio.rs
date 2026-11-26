//! #289 step 2 on the bus (`program_bus_audio.rs`): decide once, read twice.
//! Each committed boundary's audio goes to the audio side in commit order,
//! the job keeps its picture; a cut still lands on B + 2 for a click in
//! (B, B + 1) while the sources commit a slot ahead; the margin and the
//! line's counters on the status. Reuses the #209 rig (`program_bus_tests.rs`).
//! Wired via `#[cfg(test)] #[path = "program_bus_tests_audio.rs"] mod tests_audio;`.

use super::tests::{MS, SRC_A, SRC_B, b, frame, job};
use super::*;
use crate::playback::program_audio::{AudioJob, Limited};
use crate::playback::program_output_timing::BoundaryMarks;

/// A on program, both A and B offering `ks` (B one slot ahead of its
/// stamps' real time does not matter to the core: it reads no clock).
fn offer_both(core: &mut ProgramCore, ks: std::ops::RangeInclusive<usize>) {
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in ks {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
        core.offer(SRC_B, job(8, &fb, b(k), 0.2));
    }
}

/// The cut boundary for a click at `b(5) + 5 ms` after both sources
/// offered through `b(last)`.
fn cut_after(lead_slots: i64, last: usize) -> i64 {
    let mut core = ProgramCore::new().with_audio_ahead(lead_slots);
    core.select_initial(SRC_A);
    offer_both(&mut core, 1..=last);
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    core.status().cut_boundary_100ns.expect("a cut boundary")
}

#[test]
fn a_cut_lands_on_the_same_real_boundary_with_the_sources_a_slot_ahead() {
    // No lead: the sources are at b(5) when the operator clicks in
    // (b(5), b(6)); a cut lands one slot after the next boundary, b(7).
    assert_eq!(cut_after(0, 5), b(7));
    // A slot ahead, the sources already committed b(6) by then: the cut
    // still lands on b(7), never a slot later.
    assert_eq!(cut_after(1, 6), b(7));
    assert_eq!(
        cut_after(1, 5),
        b(6),
        "strictly after the newest stamp seen"
    );
}

#[test]
fn a_cut_with_nothing_seen_reads_the_clock_a_lead_ahead() {
    for lead_slots in [0, 1] {
        let mut core = ProgramCore::new().with_audio_ahead(lead_slots);
        assert!(core.cut(SRC_B, b(5) + 5 * MS));
        assert_eq!(
            core.status().cut_boundary_100ns,
            Some(b(7)),
            "lead {lead_slots}: B + 2 for a click in (B, B + 1)"
        );
    }
}

#[test]
fn every_committed_boundary_s_audio_reaches_the_audio_side_in_order() {
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(SRC_A);
    let fa = frame(4, 2);
    core.offer(SRC_A, job(4, &fa, b(1), 0.1));
    core.offer(SRC_A, job(4, &fa, b(3), 0.3)); // b(2) is missed: A passed it
    assert_eq!(core.audio_queued(), 3);
    let a1 = core.take_audio().expect("b(1)");
    assert!(
        matches!(&a1, AudioJob::Source { stamp_100ns, audio } if *stamp_100ns == b(1) && audio[0].data[0] == 0.1)
    );
    assert!(
        matches!(core.take_audio(), Some(AudioJob::Standby { stamp_100ns }) if stamp_100ns == b(2))
    );
    assert!(
        matches!(core.take_audio(), Some(AudioJob::Source { stamp_100ns, .. }) if stamp_100ns == b(3))
    );
    assert!(core.take_audio().is_none());
    // The sender's jobs: the same boundaries, the pictures, no audio.
    for k in [1, 2, 3] {
        let job = core.take().expect("the job");
        assert_eq!(job.stamp_100ns(), b(k));
        if let ProgramJob::Source(j) = job {
            assert!(j.audio.is_empty());
            assert_eq!(j.width, 4);
        }
    }
}

#[test]
fn a_fade_is_decided_once_for_both_readers() {
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(SRC_A);
    core.set_transition(crate::playback::program_transition::TransitionSpec::fade(
        300,
        crate::playback::program_transition::SpecSource::Setting,
    ));
    offer_both(&mut core, 1..=6);
    assert!(core.cut(SRC_B, b(6) + 5 * MS));
    offer_both(&mut core, 7..=12);
    let mut audio = Vec::new();
    while let Some(job) = core.take_audio() {
        if let AudioJob::Mix {
            stamp_100ns,
            first,
            total,
            ..
        } = job
        {
            audio.push((stamp_100ns, first, total));
        }
    }
    let mut pictures = Vec::new();
    while let Some(job) = core.take() {
        if let ProgramJob::Mix(mix) = job {
            let (first, total) = mix.sample_span(1600);
            pictures.push((mix.stamp_100ns, first, total));
        }
    }
    assert!(!audio.is_empty(), "the fade mixed boundaries");
    assert_eq!(audio, pictures, "the same boundaries, the same weights");
}

#[test]
fn an_audio_side_a_queue_behind_drops_its_oldest_job_counted() {
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(SRC_A);
    let fa = frame(4, 2);
    for k in 1..=PROGRAM_QUEUE_BOUND + 2 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1));
        core.take();
    }
    assert_eq!(core.status().health.audio.coalesced, 2);
    let first = core.take_audio().expect("the oldest kept");
    assert_eq!(first.stamp_100ns(), b(3));
}

#[test]
fn off_the_audio_stays_in_the_job() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.offer(SRC_A, job(4, &frame(4, 2), b(1), 0.1));
    assert_eq!(core.audio_queued(), 0);
    assert!(core.take_audio().is_none());
    let Some(ProgramJob::Source(j)) = core.take() else {
        panic!("b(1)")
    };
    assert_eq!(j.audio[0].data[0], 0.1);
    assert_eq!(core.status().health.audio.lead_slots, 0);
}

#[test]
fn the_status_carries_the_margin_and_the_line() {
    let bus = ProgramBus::new().with_audio_ahead(1);
    bus.record_timing(&BoundaryMarks {
        stamp_100ns: b(4),
        fed_100ns: b(4) - 250_000,
        ..BoundaryMarks::default()
    });
    bus.record_timing(&BoundaryMarks {
        stamp_100ns: b(5),
        fed_100ns: b(5) + 30,
        ..BoundaryMarks::default()
    });
    bus.audio_line().push(Limited {
        stamp_100ns: b(1),
        audio: None,
        fed_100ns: 0,
        limited_frames: 0,
    });
    let _ = bus.audio_line().take(b(2), std::time::Duration::ZERO);
    let audio = bus.status().health.audio;
    assert_eq!(audio.lead_slots, 1);
    assert_eq!(audio.boundaries_ahead, 1);
    assert_eq!(audio.margin_us_min, Some(-3));
    assert_eq!((audio.line.stale, audio.line.missing), (1, 1));
}

/// The clock fallback reads exactly the lead ahead: a click one 100 ns
/// tick before the lead puts it on a boundary still lands there.
#[test]
fn the_clock_fallback_adds_exactly_one_slot() {
    let mut core = ProgramCore::new().with_audio_ahead(1);
    assert!(core.cut(SRC_B, b(7) - 333_334));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
}

/// A source that does NOT commit ahead (the NDI input "OBS manuál", the
/// program's own fills) keeps the whole cut lead: a click in (B, B + 1)
/// lands on B + 2 there too, never a slot early.
#[test]
fn a_cut_between_sources_not_ahead_keeps_the_whole_lead() {
    let input = sp_core::config::PROGRAM_INPUT_ID;
    let blank = sp_core::config::PROGRAM_BLANK_ID;
    let fa = frame(4, 2);
    // The input on program, offering its capture boundary b(5).
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(input);
    core.offer(input, job(4, &fa, b(5), 0.1));
    assert!(core.cut(blank, b(5) + 5 * MS));
    assert_eq!(
        core.status().cut_boundary_100ns,
        Some(b(7)),
        "input -> Blank"
    );
    // Blank on program: only the program's fills, committed at b(5).
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(blank);
    core.release(b(5) + MS);
    assert!(core.cut(input, b(5) + 5 * MS));
    assert_eq!(
        core.status().cut_boundary_100ns,
        Some(b(7)),
        "Blank -> input"
    );
    // A playlist a slot ahead (b(6)) and the input at b(5): still b(7).
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(input);
    core.offer(input, job(4, &fa, b(5), 0.1));
    core.touch(SRC_A, b(6));
    assert!(core.cut(SRC_A, b(5) + 5 * MS));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(7)), "input -> A");
}

/// The program's last committed boundary counts with its source's lead:
/// a playlist's forward a slot ahead, a fill none.
#[test]
fn the_last_committed_boundary_carries_its_source_s_lead() {
    let fa = frame(4, 2);
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.select_initial(SRC_A);
    core.offer(SRC_A, job(4, &fa, b(6), 0.1));
    core.last_offer.clear(); // only the program's own last boundary is seen
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert_eq!(
        core.status().cut_boundary_100ns,
        Some(b(7)),
        "a forward: ahead"
    );
    let mut core = ProgramCore::new().with_audio_ahead(1);
    core.release(b(5) + MS); // nobody selected: the program fills b(5)
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert_eq!(
        core.status().cut_boundary_100ns,
        Some(b(7)),
        "a fill: not ahead"
    );
}

/// The cut's own answer carries the line's counters, like the status.
#[test]
fn the_cut_answer_carries_the_line() {
    let bus = ProgramBus::new().with_audio_ahead(1);
    let _ = bus.audio_line().take(b(2), std::time::Duration::ZERO);
    let answer = bus.cut(SRC_A, b(5), None);
    assert_eq!(answer.health.audio.line.missing, 1);
}
