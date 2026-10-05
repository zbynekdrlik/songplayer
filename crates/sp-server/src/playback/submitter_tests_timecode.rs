//! Genlock stamping + holdover tests for [`FrameSubmitter`] (#146, #203). An
//! injected [`WallClock`] makes the stamps deterministic without any `sleep`.
//! Wired via
//! `#[cfg(test)] #[path = "submitter_tests_timecode.rs"] mod submitter_tests_timecode;`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::PacedSink;
use crate::playback::wallclock::WallClock;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use std::sync::Arc;

fn audio(tc_ignored: Option<i64>) -> Vec<AudioFrame> {
    vec![AudioFrame {
        data: vec![0.1, 0.2, 0.3, 0.4],
        channels: 2,
        sample_rate: 48000,
        // The submitter re-stamps audio at submission; this incoming value
        // is deliberately whatever — the test asserts the submitter's stamp.
        timecode_100ns: tc_ignored,
    }]
}

#[test]
fn audio_is_submitted_before_video() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "G2", false, false).unwrap();
    let mut sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, WallClock::fixed(1_000_000));

    sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &audio(None), 333_333, 333_333);

    let calls = backend.calls();
    let idx_audio = calls
        .iter()
        .position(|c| c.starts_with("send_audio"))
        .expect("audio must be sent");
    let idx_video = calls
        .iter()
        .position(|c| c.starts_with("send_video_async"))
        .expect("video must be sent");
    assert!(
        idx_audio < idx_video,
        "audio must be submitted before video: {calls:#?}"
    );
}

/// A [`ClockSource`] that counts realtime samples — proves `new_with_wallclock`
/// wires the injected clock directly (no throwaway `WallClock::system()`, no
/// re-sample on construction) and that the submit read path is monotonic-only
/// (#146 follow-up).
struct SampleCountingClock {
    base: std::time::Instant,
    utc_100ns: i64,
    samples: std::sync::atomic::AtomicU64,
}

impl SampleCountingClock {
    fn new(utc_100ns: i64) -> Arc<Self> {
        Arc::new(Self {
            base: std::time::Instant::now(),
            utc_100ns,
            samples: std::sync::atomic::AtomicU64::new(0),
        })
    }
    fn samples(&self) -> u64 {
        self.samples.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::playback::wallclock::ClockSource for Arc<SampleCountingClock> {
    fn sample(&self) -> (std::time::Instant, i64) {
        self.samples
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.base, self.utc_100ns)
    }
    fn now_monotonic(&self) -> std::time::Instant {
        self.base
    }
}

#[test]
fn new_with_wallclock_uses_the_injected_clock_directly() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "IC", true, false).unwrap();

    let clock = SampleCountingClock::new(7_000_000);
    let wall = WallClock::new(Box::new(clock.clone()));
    // WallClock::new seeded the anchor with exactly one realtime sample.
    assert_eq!(clock.samples(), 1);

    let mut sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, wall);
    // Construction must NOT build a throwaway system clock nor re-sample the
    // injected clock — the injected clock is used directly.
    assert_eq!(
        clock.samples(),
        1,
        "new_with_wallclock must use the injected clock directly"
    );

    // A boundary submit reads no clock at all: the pair arrives stamped, and
    // the wall only lends its relabel registry (K = 0 on a private one), so
    // the wire stamp is the boundary itself.
    let b = floor_boundary_100ns(7_000_000, GENLOCK_GRID_FPS);
    sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &[], b, b);
    assert_eq!(clock.samples(), 1, "a boundary submit samples nothing");
    assert_eq!(
        backend.video_timecodes(),
        vec![b],
        "the boundary goes out as stamped"
    );
}

#[test]
fn holdover_keeps_the_submitted_frame_alive_across_the_async_call() {
    // #203: the async submit sends by borrowed slice and keeps the SAME
    // allocation in `prev_frame` so the SDK's retained pointer stays valid until
    // the next submit. The holdover buffer's pointer MUST equal the exact slice
    // the backend received — sending one allocation while holding another is the
    // use-after-free this test guards against.
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "HO", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);

    let data = SharedFrame::new(vec![3u8; 4 * 2 * 3 / 2]);
    sub.submit_frame_at_boundary_owned(4, 2, 4, data, &[], 333_333, 333_333);

    let (recv_ptr, recv_len) = backend
        .last_async_video_slice()
        .expect("an async video send happened");
    let held = sub
        .prev_frame
        .as_ref()
        .expect("the holdover retained the submitted frame");
    assert_eq!(
        held.as_ptr() as usize,
        recv_ptr,
        "prev_frame must hold the EXACT buffer the SDK still points at"
    );
    assert_eq!(held.len(), recv_len, "and its full length");

    // A second submit releases the first allocation and holds the second.
    let data2 = SharedFrame::new(vec![9u8; 4 * 2 * 3 / 2]);
    sub.submit_frame_at_boundary_owned(4, 2, 4, data2, &[], 666_666, 666_666);
    let (recv_ptr2, _) = backend.last_async_video_slice().unwrap();
    let held2 = sub.prev_frame.as_ref().unwrap();
    assert_eq!(
        held2.as_ptr() as usize,
        recv_ptr2,
        "the holdover now tracks the second frame's allocation"
    );
}

#[test]
fn frame_submitter_submit_shared_is_zero_copy_via_the_owned_path() {
    // #203: FrameSubmitter overrides PacedSink::submit_shared to MOVE the shared
    // handle into the async holdover — the SDK receives that exact allocation and
    // it becomes prev_frame, with no to_vec copy.
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "SS", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);

    let frame = SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2]);
    let src_ptr = frame.as_ptr() as usize;
    sub.submit_shared(4, 2, 4, frame, &[], 333_333, 333_333);

    let calls = backend.calls();
    assert!(
        calls
            .iter()
            .any(|c| c == "send_video_async(42,NV12,4x2,stride=4,30/1)"),
        "submit_shared must send an NV12 async video frame: {calls:#?}"
    );
    assert_eq!(
        backend.last_async_video_slice().unwrap().0,
        src_ptr,
        "the SDK receives the SAME allocation — no copy"
    );
    assert_eq!(
        sub.prev_frame.as_ref().unwrap().as_ptr() as usize,
        src_ptr,
        "and the holdover keeps that same allocation"
    );
    assert_eq!(sub.frames_submitted_total(), 1);
}

#[test]
fn paced_sink_emit_submits_the_pacer_frame_without_a_pixel_copy() {
    // #147 round 10: `impl PacedSink for FrameSubmitter::emit` used to copy the
    // pacer's frame (`to_vec`, a fresh 5.5 MB allocation per 1440p frame). It
    // now moves an Arc clone into the async holdover, like the handoff does.
    // The pacer's frame stays alive for the whole test, so a copy can never
    // land on its address by allocator coincidence.
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "EZ", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    let frame = crate::playback::pacer::PacedFrame {
        pts_ns: 0,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2]),
        audio: Vec::new(),
    };
    let src_ptr = frame.video.as_ptr() as usize;

    PacedSink::emit(&mut sub, &frame, &[], 333_333, 333_333);

    assert_eq!(
        backend.last_async_video_slice(),
        Some((src_ptr, 12)),
        "the SDK receives the pacer's own allocation — no copy"
    );
    assert!(
        sub.prev_frame.as_ref().unwrap().ptr_eq(&frame.video),
        "the holdover shares the pacer's allocation (an Arc bump)"
    );
    assert_eq!(&frame.video[..], &[16u8; 12][..], "pacer pixels untouched");
    assert_eq!(sub.frames_submitted_total(), 1);
}

#[test]
fn borrowed_boundary_submit_copies_into_a_recycled_pool_buffer() {
    // #147 round 10: the borrow API (`submit_frame_at_boundary(&[u8])`) still
    // has to copy — the caller keeps its slice — but into a RECYCLED pool
    // buffer (`SharedFrame::copy_from_slice`), never a fresh `to_vec`. The
    // recycled buffer stays alive in the pool until the submit takes it, and a
    // unique length keeps this size class private to this test.
    use sp_decoder::frame_pool::{recycle, take};
    const LEN: usize = 1_300_021;
    let mut spare = take(LEN);
    spare.resize(LEN, 0);
    let spare_ptr = spare.as_ptr() as usize;
    recycle(spare);

    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "BC", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    let data: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();

    sub.submit_frame_at_boundary(4, 2, 4, &data, &[], 333_333, 333_333);

    let held = sub.prev_frame.as_ref().unwrap();
    assert_eq!(
        held.as_ptr() as usize,
        spare_ptr,
        "the copy reuses the recycled buffer instead of allocating"
    );
    assert_eq!(&held[..], &data[..], "with the caller's exact bytes");
    assert_eq!(backend.last_async_video_slice(), Some((spare_ptr, LEN)));
}
