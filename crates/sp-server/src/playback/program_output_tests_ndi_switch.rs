//! #279 P1b review round 5: NDI `SP-program`'s switches with the
//! `program-max` thread in the loop (the worker on the fake GPU, sharing
//! the sender's hand-off), and the lines they log.
//!
//! - F1: the thread usually reads the setting's switch to the canvas
//!   BEFORE the `SP-program` sender serves the next boundary. It switches
//!   its pass off (MAX on), or releases the compositor (MAX off), first.
//!   The held pair must still go out with its own read-back picture: the
//!   sender alone lets the feed's pictures go, after its flush.
//! - ROZHODNUTÉ 6233825414: the automatic fallback to the canvas is a
//!   WARN, the way back to the GPU an INFO.
//!
//! Wired via `#[cfg(test)] #[path = "program_output_tests_ndi_switch.rs"] mod tests_ndi_switch;`.

use std::sync::Arc;
use std::time::Instant;

use sp_core::config::NdiSource;
use sp_ndi::NdiSender;
use sp_ndi::test_util::MockNdiBackend;

use super::ProgramOutput;
use super::tests_order::{at, pair, samples};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_max::MaxOut;
use crate::playback::program_max_worker::MaxWorker;
use crate::playback::program_max_worker::tests::{
    FAKE_UYVY_BYTES, FakeGpu, fake_uyvy, picture, plain,
};
use crate::test_log::{Captured, capturing};

/// The canvas: 2×2, so a whole UYVY picture is exactly the fake GPU's
/// 8-byte readback.
const W: u32 = 2;
const H: u32 = 2;
const _: () = assert!((W * 2 * H) as usize == FAKE_UYVY_BYTES);

/// The `SP-program` sender on the 2×2 canvas with MAX's hand-off, NDI from
/// the GPU, MAX on or off.
fn output(
    max_on: bool,
) -> (
    Arc<MockNdiBackend>,
    ProgramOutput<MockNdiBackend>,
    Arc<MaxOut>,
) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let max = Arc::new(MaxOut::new());
    max.set_enabled(max_on);
    max.set_ndi_source(NdiSource::Gpu);
    let out = ProgramOutput::new(sender, W, H).with_max(max.clone());
    (backend, out, max)
}

/// A forwarded 2×2 pair (a canvas picture) on boundary `k`.
fn source(k: usize) -> ProgramJob {
    ProgramJob::Source(pair(W, at(k), at(k) + 77, samples(0.25, 1e-5)))
}

/// Boundaries 0 and 1 from the GPU (the thread reads each back before the
/// sender serves it), then the setting's switch to the canvas, which the
/// thread sees first: with MAX on it serves boundary 2 with NDI no longer
/// wanted (its pass goes off), with MAX off nothing wants the compositor
/// and it releases it (`MaxNext::Release`). Then the sender serves
/// boundary 2. Returns the mock, the hand-off and the fake GPU.
fn switch_with_the_thread_first(max_on: bool) -> (Arc<MockNdiBackend>, Arc<MaxOut>, FakeGpu) {
    let (backend, mut out, max) = output(max_on);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let a = picture(4, 2);
    let t0 = Instant::now();
    for k in 0..2 {
        worker.serve(&plain(at(k), &a), t0);
        out.submit(source(k));
    }
    max.set_ndi_source(NdiSource::Cpu);
    if max_on {
        worker.serve(&plain(at(2), &a), t0);
    } else {
        worker.release(t0);
    }
    out.submit(source(2));
    // The worker borrows the hand-off: it goes first (its compositor and
    // senders, if any, with it).
    drop(worker);
    (backend, max, gpu)
}

/// MAX on: the thread switched its pass off before the sender's boundary 2,
/// and boundary 1's held pair still went out with its own readback (the
/// thread's second compose): no repeat, contiguous stamps. (The clear
/// after the flush is pinned in `program_output_tests_ndi.rs`.)
#[test]
fn the_held_pair_keeps_its_picture_when_the_thread_sees_the_switch_first() {
    let (backend, max, gpu) = switch_with_the_thread_first(true);
    assert!(
        gpu.log().order.contains(&"ndi off"),
        "the thread switched its pass off first"
    );
    assert_eq!(backend.video_timecodes(), [at(0), at(1), at(2)]);
    let status = max.ndi_status();
    assert_eq!((status.switches, status.repeats), (2, 0), "{status:?}");
}

/// MAX off: the thread released the compositor before the sender's
/// boundary 2; boundary 1's held pair still went out with its own
/// readback.
#[test]
fn the_held_pair_keeps_its_picture_when_the_thread_releases_first() {
    let (backend, max, gpu) = switch_with_the_thread_first(false);
    assert_eq!(gpu.log().drops, ["compositor"], "released first");
    assert_eq!(backend.video_timecodes(), [at(0), at(1), at(2)]);
    let status = max.ndi_status();
    assert_eq!((status.switches, status.repeats), (2, 0), "{status:?}");
    assert_eq!(
        backend.last_async_video_slice().map(|(_, len)| len),
        Some(6),
        "boundary 2 on the NV12 canvas, after the flush"
    );
}

/// ROZHODNUTÉ 6233825414: NDI's automatic fallback to the canvas is a WARN
/// (a fault the operator should see), with its reason; the way back to
/// the GPU (after 30 on-time readbacks) is an INFO, like the first switch.
/// The lines are rate-limited to one per 5 s of the timeline (150 grid
/// slots), so the run spaces them: the GPU from boundary 0, the readback
/// stopping after 199 (stale at 230), back on time from 230 (on the GPU at
/// 260, said at the first boundary 5 s after the WARN).
#[test]
fn the_fallback_is_a_warn_and_the_way_back_an_info() {
    let cap = Captured::default();
    let max = tracing::subscriber::with_default(capturing(&cap), || {
        let (_backend, mut out, max) = output(false);
        let picture = SharedFrame::new(fake_uyvy(1));
        for k in 0..200 {
            max.ndi.push(at(k), picture.clone());
            out.submit(source(k));
        }
        for k in 200..=230 {
            out.submit(source(k));
        }
        max.ndi.push(at(230), picture.clone());
        for k in 231..=400 {
            max.ndi.push(at(k), picture.clone());
            out.submit(source(k));
        }
        max
    });
    let status = max.ndi_status();
    assert_eq!((status.source, status.switches), ("gpu", 3), "{status:?}");
    let falls = cap.lines_with("falls back to the CPU canvas");
    assert_eq!(falls.len(), 1, "{falls:?}");
    assert!(
        falls[0].contains(" WARN ") && falls[0].contains("stale"),
        "{falls:?}"
    );
    let back = cap.lines_with("now from the GPU readback");
    assert_eq!(back.len(), 2, "the first switch and the way back: {back:?}");
    assert!(back.iter().all(|line| line.contains(" INFO ")), "{back:?}");
}
