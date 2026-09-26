//! #209 program bus: the cut, the reorder buffer and the standby fill, driven
//! through the pure [`ProgramCore`] and a real [`ProgramOutput`] over
//! `MockNdiBackend`, so every assertion reads what the `SP-program` sender
//! actually sent. Source A's frames are 4×2 NV12, source B's 8×2, and the
//! program's own standby black is 2×2 — the `send_video_async(…,WxH,…)` call
//! strings name who owned each boundary.
//! Wired via `#[cfg(test)] #[path = "program_bus_tests.rs"] mod tests;`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_output::ProgramOutput;
use crate::playback::submit_handoff::SubmitJob;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, floor_boundary_100ns, interval_100ns, strict_next_boundary_100ns,
};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::sync::Arc;
use std::time::Duration;

/// 2026-09 in 100 ns since the epoch.
const T0: i64 = 17_900_000_000_000_000;
const MS: i64 = 10_000;
const SRC_A: i64 = 11;
const SRC_B: i64 = 22;
const SRC_C: i64 = 33;

/// The k-th grid boundary after `floor(T0)` (`b(0)` = `floor(T0)`).
fn b(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

fn grace() -> i64 {
    PROGRAM_FILL_GRACE_SLOTS * interval_100ns(GENLOCK_GRID_FPS)
}

fn frame(w: u32, h: u32) -> SharedFrame {
    SharedFrame::new(vec![0u8; (w * h * 3 / 2) as usize])
}

/// One 1600-sample stereo block carrying `level`.
fn block(level: f32) -> Vec<AudioFrame> {
    vec![AudioFrame {
        data: vec![level; 1600 * 2],
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }]
}

/// A source's boundary job: the SAME shared frame, one audio block, the audio
/// stamped 2 ms after the boundary.
fn job(w: u32, video: &SharedFrame, stamp: i64, level: f32) -> SubmitJob {
    SubmitJob {
        width: w,
        height: 2,
        stride: w,
        video: video.clone(),
        audio: block(level),
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp + 2 * MS,
    }
}

fn program() -> (Arc<MockNdiBackend>, ProgramOutput<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    (backend, ProgramOutput::new(sender, 2, 2))
}

/// Send every queued program boundary through the mock `SP-program` sender.
fn drain(core: &mut ProgramCore, out: &mut ProgramOutput<MockNdiBackend>) {
    while let Some(job) = core.take() {
        let stamp = out.submit(job, T0);
        core.record_submitted(stamp);
    }
}

/// The `WxH` of every video the program sent, in order.
fn video_dims(backend: &MockNdiBackend) -> Vec<String> {
    backend
        .calls()
        .iter()
        .filter(|c| c.starts_with("send_video_async("))
        .map(|c| c.split(',').nth(2).unwrap_or_default().to_string())
        .collect()
}

/// The send calls (audio + video) after the sender's creation, in order.
fn sends(backend: &MockNdiBackend) -> Vec<String> {
    backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_audio(") || c.starts_with("send_video_async("))
        .collect()
}

fn dims(spec: &[(&str, usize)]) -> Vec<String> {
    spec.iter()
        .flat_map(|&(d, n)| std::iter::repeat_n(d.to_string(), n))
        .collect()
}

fn stamps(range: std::ops::RangeInclusive<usize>) -> Vec<i64> {
    range.map(b).collect()
}

#[test]
fn a_cut_is_contiguous_with_one_pair_per_boundary() {
    // A plays on program; B plays too. Both offer every boundary b(1)..b(12);
    // at b(5)+5 ms the operator cuts to B. The cut lands on the next boundary
    // (b(6)) + 1 slot = b(7): A keeps b(1)..b(6), B owns b(7) on.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=12 {
        let now = b(k) + 5 * MS;
        let (a_owns, b_owns) = (k < 7, k >= 7);
        assert_eq!(
            core.offer(SRC_A, job(4, &fa, b(k), 0.1), now),
            if a_owns {
                OfferOutcome::Accepted
            } else {
                OfferOutcome::NotOwner
            },
            "A at b({k})"
        );
        assert_eq!(
            core.offer(SRC_B, job(8, &fb, b(k), 0.2), now),
            if b_owns {
                OfferOutcome::Accepted
            } else {
                OfferOutcome::NotOwner
            },
            "B at b({k})"
        );
        if k == 5 {
            assert!(core.cut(SRC_B, now), "a real cut is recorded");
            let st = core.status();
            assert_eq!(st.cut_boundary_100ns, Some(b(7)), "next boundary + 1 slot");
            assert_eq!((st.source, st.previous), (Some(SRC_B), Some(SRC_A)));
        }
        drain(&mut core, &mut out);
    }
    assert_eq!(
        backend.video_timecodes(),
        stamps(1..=12),
        "one frame per boundary, contiguous across the cut"
    );
    assert_eq!(video_dims(&backend), dims(&[("4x2", 6), ("8x2", 6)]));
    let sends = sends(&backend);
    assert_eq!(
        sends.len(),
        24,
        "exactly one audio + one video per boundary"
    );
    for pair in sends.chunks(2) {
        assert!(pair[0].starts_with("send_audio(42,sr=48000,ch=2,spc=1600)"));
        assert!(pair[1].starts_with("send_video_async(42,NV12,"));
    }
    // The program forwards the source's own audio stamps, unchanged.
    let audio: Vec<i64> = (1..=12).map(|k| b(k) + 2 * MS).collect();
    assert_eq!(backend.audio_timecodes(), audio);
    let st = core.status();
    assert_eq!(st.health.forwarded, 12);
    assert_eq!(st.health.filled, 0);
    assert_eq!(st.health.late_dropped, 0);
    assert_eq!(st.health.cuts, 1);
    assert_eq!(st.health.submitted, 12);
    assert_eq!(st.health.last_stamp_100ns, b(12));
    assert_eq!(
        (st.source, st.previous),
        (Some(SRC_B), None),
        "once the cut boundary is served, A no longer owns anything"
    );
    assert!(!core.is_candidate(SRC_A), "A is pruned after the cut");
    assert!(core.is_candidate(SRC_B));
}

#[test]
fn the_new_source_waits_for_the_old_sources_last_boundary() {
    // A's submit thread runs late: B's first owned frame (b(7)) arrives before
    // A's last one (b(6)). The program holds b(7) until b(6) is in — no black,
    // no hole, no out-of-order send.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + 5 * MS);
    }
    core.cut(SRC_B, b(5) + 5 * MS);
    let early = core.offer(SRC_B, job(8, &fb, b(7), 0.2), b(7) + 2 * MS);
    assert_eq!(early, OfferOutcome::Accepted);
    drain(&mut core, &mut out);
    assert_eq!(
        backend.video_timecodes(),
        stamps(1..=5),
        "b(7) waits for b(6)"
    );

    let late_a = core.offer(SRC_A, job(4, &fa, b(6), 0.1), b(7) + 20 * MS);
    assert_eq!(late_a, OfferOutcome::Accepted);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), stamps(1..=7));
    assert_eq!(video_dims(&backend), dims(&[("4x2", 6), ("8x2", 1)]));
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(7), 0.1), b(7) + 21 * MS),
        OfferOutcome::NotOwner
    );
    assert_eq!(core.status().health.filled, 0, "nothing was filled");
}

#[test]
fn a_cut_to_an_idle_source_carries_its_standby_pair() {
    // B is idle: its paced output emits its own #147 standby pair (B's NV12
    // black + one silent block) every boundary. After the cut the program
    // carries exactly that pair — B's own allocation, not the program's fill.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let fa = frame(4, 2);
    let b_black = frame(6, 2);
    for k in 1..=10 {
        let now = b(k) + 5 * MS;
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), now);
        core.offer(SRC_B, job(6, &b_black, b(k), 0.0), now);
        if k == 5 {
            core.cut(SRC_B, now);
        }
        drain(&mut core, &mut out);
    }
    assert_eq!(backend.video_timecodes(), stamps(1..=10));
    assert_eq!(video_dims(&backend), dims(&[("4x2", 6), ("6x2", 4)]));
    let last = backend.last_async_video_slice().expect("a video was sent");
    assert_eq!(
        last,
        (b_black.as_ptr() as usize, b_black.len()),
        "the program sends B's own standby black (zero copy)"
    );
    assert!(
        backend.last_audio_planar().iter().all(|&s| s == 0.0),
        "and B's silent block"
    );
    assert_eq!(
        sends(&backend).len(),
        20,
        "one audio + one video per boundary"
    );
    assert_eq!(core.status().health.filled, 0, "no program fill was needed");
}

#[test]
fn a_source_that_stalls_at_the_cut_boundary_is_covered_by_the_standby_fill() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + 5 * MS);
    }
    core.cut(SRC_B, b(5) + 5 * MS); // cut boundary b(7)
    // A stalls: its b(6) never comes in time. B delivers b(7), b(8).
    core.offer(SRC_B, job(8, &fb, b(7), 0.2), b(7) + 5 * MS);
    assert_eq!(
        core.offer(SRC_B, job(8, &fb, b(7), 0.2), b(7) + 6 * MS),
        OfferOutcome::Late,
        "a duplicate of a waiting boundary is dropped"
    );
    core.offer(SRC_B, job(8, &fb, b(8), 0.2), b(8) + 5 * MS);
    core.release(b(6) + grace() - 1);
    drain(&mut core, &mut out);
    assert_eq!(
        backend.video_timecodes(),
        stamps(1..=5),
        "A is still inside its grace: nothing after b(5) yet"
    );

    core.release(b(6) + grace());
    drain(&mut core, &mut out);
    assert_eq!(
        backend.video_timecodes(),
        stamps(1..=8),
        "b(6) filled, B on"
    );
    assert_eq!(
        video_dims(&backend),
        dims(&[("4x2", 5), ("2x2", 1), ("8x2", 2)]),
        "b(6) is the program's own standby black"
    );
    let sends = sends(&backend);
    assert_eq!(sends.len(), 16, "the fill is a full audio + video pair");
    assert_eq!(sends[10], "send_audio(42,sr=48000,ch=2,spc=1600)");
    // The fill's audio is stamped at the emit instant (`drain` passes T0).
    assert_eq!(backend.audio_timecodes()[5], T0);

    // A's stalled b(6) finally arrives: the boundary was served and A no longer
    // owns anything, so it is not forwarded — no double, no out-of-order send.
    assert!(!core.is_candidate(SRC_A));
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(6), 0.1), b(9) + 5 * MS),
        OfferOutcome::NotOwner
    );
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes().len(), 8);
    let h = core.status().health;
    assert_eq!((h.filled, h.forwarded, h.late_dropped), (1, 7, 1));
}

#[test]
fn a_new_source_whose_first_frame_after_the_cut_is_slow_is_waited_for() {
    // Every paced source reports its progress on every boundary through
    // `program_copy`, program candidate or not. So B is known to be live when
    // the sender checks the cut boundary b(7) before B's first post-cut frame
    // is in: b(7) waits for B's frame instead of going black.
    let bus = ProgramBus::new();
    bus.select_initial(SRC_A);
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=6 {
        let now = b(k) + 5 * MS;
        let a = job(4, &fa, b(k), 0.1);
        if let Some(copy) = program_copy(&bus, SRC_A, &a) {
            bus.offer(SRC_A, copy, now);
        }
        if k <= 5 {
            let bj = job(8, &fb, b(k), 0.2);
            assert!(program_copy(&bus, SRC_B, &bj).is_none(), "B is off program");
        }
        if k == 5 {
            bus.cut(SRC_B, now); // cut boundary b(7)
        }
    }
    // B's b(6) submit is slow: the sender's b(7) check comes first.
    bus.release_due(b(7) + MS);
    let bj = job(8, &fb, b(7), 0.2);
    let copy = program_copy(&bus, SRC_B, &bj).expect("B is on program now");
    assert_eq!(
        bus.offer(SRC_B, copy, b(7) + 10 * MS),
        OfferOutcome::Accepted
    );

    let mut sent = Vec::new();
    while let Take::Job(job) = bus.take_timeout(Duration::ZERO) {
        let width = match &job {
            ProgramJob::Source(j) => j.width,
            ProgramJob::Standby { .. } => 0,
        };
        sent.push((job.stamp_100ns(), width));
    }
    let want: Vec<(i64, u32)> = (1..=7).map(|k| (b(k), if k < 7 { 4 } else { 8 })).collect();
    assert_eq!(sent, want, "b(7) is B's frame, not the program's black");
    let h = bus.status().health;
    assert_eq!((h.filled, h.late_dropped), (0, 0));
}

#[test]
fn a_cut_follows_the_sources_clock_when_the_api_clock_lags() {
    // The API reads the realtime clock; the sources stamp on their own slewed
    // wall. After a UTC step the realtime clock can lag the stamps: the cut
    // must still land on the boundary after next of what the sources emit.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let fa = frame(4, 2);
    for k in 1..=10 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + MS);
    }
    assert!(core.cut(SRC_B, b(5)));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(12)));

    // An API clock AHEAD of the sources (a forward UTC step the stamp walls are
    // still slewing in) must not delay the cut either: the bus cuts in the
    // sources' stamp domain, the caller's clock is only the fallback.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.offer(SRC_A, job(4, &fa, b(1), 0.1), b(1) + MS);
    assert!(core.cut(SRC_B, b(20)));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(3)));
}

#[test]
fn a_cut_ignores_the_progress_of_sources_it_does_not_involve() {
    // C (off program, never involved) once stamped far ahead; only the program
    // sources, the source cut to and the program's own progress place a cut.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let fa = frame(4, 2);
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + MS);
    }
    assert!(!core.touch(SRC_C, b(40)));
    assert!(!core.touch(SRC_B, b(5)));
    assert!(core.cut(SRC_B, b(5) + 5 * MS));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
}

#[test]
fn an_offer_never_decides_a_missed_boundary_on_the_sources_own_clock() {
    // The paced submit thread passes its own per-song wall, which after a
    // forward UTC step can read hundreds of ms ahead of the stamps. Only the
    // program sender (on its own wall) declares a boundary missed by time;
    // the offer path forwards, and fills only a gap the owner is already past.
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let fa = frame(4, 2);
    for k in 1..=5 {
        let ahead = b(k) + 500 * MS;
        assert_eq!(
            core.offer(SRC_A, job(4, &fa, b(k), 0.1), ahead),
            OfferOutcome::Accepted,
            "b({k})"
        );
        drain(&mut core, &mut out);
    }
    assert_eq!(backend.video_timecodes(), stamps(1..=5));
    assert_eq!(video_dims(&backend), dims(&[("4x2", 5)]));
    let h = core.status().health;
    assert_eq!((h.filled, h.late_dropped, h.resyncs), (0, 0, 0));
}

#[test]
fn a_source_that_jumps_more_than_eight_slots_resyncs_on_the_offer_path() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let fa = frame(4, 2);
    core.offer(SRC_A, job(4, &fa, b(1), 0.1), b(1) + MS);
    core.offer(SRC_A, job(4, &fa, b(20), 0.1), b(20) + MS);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), vec![b(1), b(20)]);
    let h = core.status().health;
    assert_eq!((h.resyncs, h.filled, h.forwarded), (1, 0, 2));
}

#[test]
fn cutting_back_before_the_cut_boundary_cancels_the_cut() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(core.cut(SRC_B, b(5) + 5 * MS)); // b(7)
    assert!(core.cut(SRC_A, b(5) + 6 * MS)); // same slot: back to A
    assert_eq!(core.owner_of(b(7)), Some(SRC_A));
    assert!(!core.is_candidate(SRC_B));
    let st = core.status();
    assert_eq!((st.source, st.previous), (Some(SRC_A), None));
    assert_eq!(st.cut_boundary_100ns, None, "A owns every boundary again");
    assert_eq!(st.health.cuts, 2);
}

#[test]
fn with_no_source_every_boundary_is_filled_on_time() {
    let mut core = ProgramCore::new();
    let (backend, mut out) = program();
    core.release(b(1));
    drain(&mut core, &mut out);
    core.release(b(2) + MS);
    drain(&mut core, &mut out);
    core.release(b(2) + 2 * MS); // same boundary again: nothing new
    drain(&mut core, &mut out);
    core.release(b(3) + MS);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), stamps(1..=3));
    assert_eq!(video_dims(&backend), dims(&[("2x2", 3)]));
    assert_eq!(sends(&backend).len(), 6);
    let st = core.status();
    assert_eq!(
        (st.source, st.previous, st.cut_boundary_100ns),
        (None, None, None)
    );
    assert_eq!(st.health.filled, 3);
}

#[test]
fn a_selected_source_that_never_offered_is_filled_on_time() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    core.release(b(1) + MS);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), vec![b(1)]);
    assert_eq!(
        core.status().cut_boundary_100ns,
        None,
        "a restored source owns every boundary"
    );
}

#[test]
fn an_owner_that_went_quiet_is_absent_after_the_live_window() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.offer(SRC_A, job(4, &frame(4, 2), b(1), 0.1), b(1) + MS);
    // b(31) = b(1) + 1 s: inside its grace at both instants below.
    let far = b(31);
    assert_eq!(far, b(1) + PROGRAM_LIVE_WINDOW_100NS);
    assert!(
        !core.fill_due(far, b(1) + PROGRAM_LIVE_WINDOW_100NS),
        "exactly one live window after its last offer A is still live"
    );
    assert!(
        core.fill_due(far, b(1) + PROGRAM_LIVE_WINDOW_100NS + 1),
        "one tick later A is absent: fill on time"
    );
}

#[test]
fn a_live_owners_boundary_is_missed_exactly_one_grace_after_it() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.offer(SRC_A, job(4, &frame(4, 2), b(1), 0.1), b(1) + MS);
    assert!(!core.fill_due(b(2), b(2) + grace() - 1));
    assert!(core.fill_due(b(2), b(2) + grace()));
}

#[test]
fn a_boundary_is_never_filled_before_it_is_reached() {
    // No owner at all: the only thing that holds the fill back is the clock.
    let core = ProgramCore::new();
    assert!(!core.fill_due(b(2), b(2) - 1));
    assert!(core.fill_due(b(2), b(2)));
}

#[test]
fn a_gap_in_the_owners_own_stream_is_filled_at_once() {
    // A's handoff coalesced b(2) and b(3) away; A already offered b(4), and one
    // source offers in stamp order, so b(2)/b(3) will never come: fill them
    // right away (well inside the grace) and forward b(4).
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let fa = frame(4, 2);
    core.offer(SRC_A, job(4, &fa, b(1), 0.1), b(1) + MS);
    core.offer(SRC_A, job(4, &fa, b(4), 0.1), b(4) + MS);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), stamps(1..=4));
    assert_eq!(
        video_dims(&backend),
        dims(&[("4x2", 1), ("2x2", 2), ("4x2", 1)])
    );
    assert_eq!(core.status().health.filled, 2);
    assert_eq!(
        core.offer(SRC_A, job(4, &fa, b(4), 0.1), b(4) + 2 * MS),
        OfferOutcome::Late,
        "a re-sent boundary that was already served is dropped"
    );
    assert_eq!(core.status().health.late_dropped, 1);
}

#[test]
fn more_than_eight_missed_slots_resync_instead_of_bursting() {
    let mut core = ProgramCore::new();
    let (backend, mut out) = program();
    core.release(b(1) + MS);
    drain(&mut core, &mut out);
    // b(2) is exactly 8 slots behind floor(now) = b(10): a catch-up burst.
    core.release(b(10) + MS);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), stamps(1..=10));
    assert_eq!(core.status().health.resyncs, 0);
    // b(11) is 10 slots behind b(21): resync onto b(21), no burst.
    core.release(b(21) + MS);
    drain(&mut core, &mut out);
    let mut want = stamps(1..=10);
    want.push(b(21));
    assert_eq!(backend.video_timecodes(), want);
    let h = core.status().health;
    assert_eq!((h.resyncs, h.filled), (1, 11));
}

#[test]
fn a_full_program_queue_coalesces_to_the_freshest_boundaries() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (backend, mut out) = program();
    let fa = frame(4, 2);
    for k in 1..=12 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + MS);
    }
    assert_eq!(core.queued(), PROGRAM_QUEUE_BOUND);
    drain(&mut core, &mut out);
    assert_eq!(backend.video_timecodes(), stamps(3..=12));
    assert_eq!(core.status().health.coalesced, 2);
}

#[test]
fn a_reorder_buffer_over_its_bound_forces_the_missing_boundary() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    let (fa, fb) = (frame(4, 2), frame(8, 2));
    for k in 1..=5 {
        core.offer(SRC_A, job(4, &fa, b(k), 0.1), b(k) + MS);
    }
    while core.take().is_some() {}
    core.cut(SRC_B, b(5) + MS); // b(7)
    let now = b(7) + MS; // A (b(6)) is live and inside its grace
    for k in 7..7 + PROGRAM_PENDING_BOUND {
        core.offer(SRC_B, job(8, &fb, b(k), 0.2), now);
    }
    assert_eq!(
        core.queued(),
        0,
        "{PROGRAM_PENDING_BOUND} waiting frames still wait"
    );
    core.offer(SRC_B, job(8, &fb, b(7 + PROGRAM_PENDING_BOUND), 0.2), now);
    let h = core.status().health;
    assert_eq!(h.filled, 1, "one more forces b(6)");
    assert_eq!(h.forwarded, 5 + PROGRAM_PENDING_BOUND as u64 + 1);
    assert_eq!(core.queued(), PROGRAM_QUEUE_BOUND);
}

#[test]
fn ownership_switches_exactly_on_the_cut_boundary() {
    let mut core = ProgramCore::new();
    assert_eq!(core.owner_of(b(1)), None);
    core.select_initial(SRC_A);
    assert!(core.cut(SRC_B, b(5)), "on a boundary the next one is b(6)");
    assert_eq!(core.status().cut_boundary_100ns, Some(b(7)));
    assert_eq!(core.owner_of(b(7)), Some(SRC_B));
    assert_eq!(core.owner_of(b(7) - 1), Some(SRC_A));
    assert_eq!(core.owner_of(i64::MIN), Some(SRC_A));
}

#[test]
fn cutting_to_the_selected_source_records_nothing() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    assert!(!core.cut(SRC_A, b(5)));
    assert_eq!(core.status().health.cuts, 0);
    assert!(core.cut(SRC_B, b(5)));
    assert!(!core.cut(SRC_B, b(5) + MS));
    assert_eq!(core.status().health.cuts, 1);
}

#[test]
fn a_later_cut_replaces_one_on_the_same_boundary_and_chains_after_an_earlier_one() {
    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.cut(SRC_B, b(5) + 5 * MS); // b(7)
    core.cut(SRC_C, b(5) + 6 * MS); // also b(7): replaces B
    assert_eq!(core.owner_of(b(6)), Some(SRC_A));
    assert_eq!(core.owner_of(b(7)), Some(SRC_C));
    assert!(!core.is_candidate(SRC_B));
    let st = core.status();
    assert_eq!((st.source, st.previous), (Some(SRC_C), Some(SRC_A)));

    let mut core = ProgramCore::new();
    core.select_initial(SRC_A);
    core.cut(SRC_B, b(5) + 5 * MS); // b(7)
    core.cut(SRC_C, b(6) + 5 * MS); // b(8)
    assert_eq!(core.owner_of(b(6)), Some(SRC_A));
    assert_eq!(core.owner_of(b(7)), Some(SRC_B));
    assert_eq!(core.owner_of(b(8)), Some(SRC_C));
    let st = core.status();
    assert_eq!((st.source, st.previous), (Some(SRC_C), Some(SRC_B)));
    assert_eq!(st.cut_boundary_100ns, Some(b(8)));
}

#[test]
fn the_bus_wakes_the_sender_and_stops_after_draining() {
    let bus = ProgramBus::new();
    assert!(matches!(bus.take_timeout(Duration::ZERO), Take::Idle));
    assert!(!bus.is_candidate(SRC_A));
    bus.select_initial(SRC_A);
    assert!(bus.is_candidate(SRC_A));
    let fa = frame(4, 2);
    let src = job(4, &fa, b(1), 0.1);
    let copy = program_copy(&bus, SRC_A, &src).expect("A can own a boundary");
    assert!(
        copy.video.ptr_eq(&fa),
        "the copy is an Arc bump of the frame"
    );
    assert!(program_copy(&bus, SRC_B, &src).is_none(), "B pays nothing");
    assert_eq!(bus.offer(SRC_A, copy, b(1) + MS), OfferOutcome::Accepted);
    match bus.take_timeout(Duration::ZERO) {
        Take::Job(job) => assert_eq!(job.stamp_100ns(), b(1)),
        _ => panic!("the forwarded boundary is queued"),
    }
    bus.release_due(b(2) + grace());
    bus.stop();
    match bus.take_timeout(Duration::ZERO) {
        Take::Job(ProgramJob::Standby { stamp_100ns }) => assert_eq!(stamp_100ns, b(2)),
        _ => panic!("a stopped bus still hands out what is queued"),
    }
    assert!(matches!(bus.take_timeout(Duration::ZERO), Take::Stopped));
    bus.record_submitted(b(2));
    bus.set_connections(3);
    let st = bus.cut(SRC_B, b(3));
    assert_eq!(st.source, Some(SRC_B));
    assert_eq!(st.health.connections, 3);
    assert_eq!(st.health.submitted, 1);
    assert_eq!(st.health.last_stamp_100ns, b(2));
    assert_eq!(bus.status(), st);
}

#[test]
fn a_waiting_sender_wakes_on_a_queued_boundary_and_on_stop() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(SRC_A);
    let long = Duration::from_secs(20);
    let waiter = {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let got = matches!(bus.take_timeout(long), Take::Job(_));
            (got, t.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    bus.offer(SRC_A, job(4, &frame(4, 2), b(1), 0.1), b(1) + MS);
    let (got, waited) = waiter.join().unwrap();
    assert!(got, "the waiter got the boundary");
    assert!(
        waited < Duration::from_secs(10),
        "woken, not timed out: {waited:?}"
    );

    let stopper = {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let stopped = matches!(bus.take_timeout(long), Take::Stopped);
            (stopped, t.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    bus.stop();
    let (stopped, waited) = stopper.join().unwrap();
    assert!(stopped);
    assert!(
        waited < Duration::from_secs(10),
        "woken by stop: {waited:?}"
    );
}

#[test]
fn default_is_an_empty_program() {
    assert_eq!(ProgramCore::default().status(), ProgramCore::new().status());
    assert_eq!(ProgramBus::default().status(), ProgramBus::new().status());
    assert_eq!(ProgramCore::default().status().source, None);
}

#[test]
fn the_installed_bus_is_the_first_one() {
    // The ONLY test that installs the process-wide bus.
    let first = Arc::new(ProgramBus::new());
    assert!(install(first.clone()));
    assert!(!install(Arc::new(ProgramBus::new())));
    assert!(Arc::ptr_eq(installed().expect("installed"), &first));
}

#[tokio::test]
async fn the_selected_source_persists_and_reloads() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let bus = ProgramBus::new();
    assert_eq!(restore_selected_source(&pool, &bus).await, None);
    assert_eq!(bus.status().source, None);

    persist_selected_source(&pool, 7).await.unwrap();
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );
    let reloaded = ProgramBus::new();
    assert_eq!(restore_selected_source(&pool, &reloaded).await, Some(7));
    assert_eq!(reloaded.status().source, Some(7));

    crate::db::models::set_setting(&pool, SETTING_PROGRAM_SOURCE, "nope")
        .await
        .unwrap();
    let bad = ProgramBus::new();
    assert_eq!(restore_selected_source(&pool, &bad).await, None);
    assert_eq!(bad.status().source, None);
}
