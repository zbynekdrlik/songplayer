//! #279 P1b: NDI `SP-program` from the GPU readback, one boundary late —
//! the delay line (`program_output_ndi.rs`) over the mock NDI, with VBAN and
//! MAX's hand-off, the readback feed filled by the test (no `program-max`
//! thread: nothing takes MAX's offers). Boundary b's audio goes to VBAN at
//! b and to NDI at b + 1 with b's stamps and b's read-back picture; a
//! missing picture repeats the last one (the black before any); a switch
//! to the GPU holds one boundary, a switch back sends the held pair first
//! (no audio block lost); after an automatic fallback the GPU comes back
//! only after 30 on-time readbacks in a row; a fade's audio is crossfaded
//! and its picture is the compositor's; the burn is painted on the UYVY
//! copy; the trace names the stamp that went out. (VBAN before any NDI
//! call in gpu mode: `program_output_tests_order.rs`.)
//! Wired via `#[cfg(test)] #[path = "program_output_tests_ndi.rs"] mod tests_ndi;`.

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sp_core::config::NdiSource;
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{FourCCVideoType, NdiSender};

use super::tests_order::{HookedNdi, at, interleaved, pair, samples};
use super::{MixRun, ProgramOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::fleet_shift;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::UYVY_BLACK_PAIR;
use crate::playback::program_burn;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_item::ProgramItem;
use crate::playback::program_max::MaxOut;
use crate::playback::program_trace::ProgramTrace;
use crate::playback::program_transition::{MixJob, crossfade_gains};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::{VbanOut, VbanTake};

/// The test canvas: 4×2 (UYVY 16 bytes, stride 8; NV12 12 bytes).
const W: u32 = 4;
const H: u32 = 2;

const CREATE: &str = "send_create_with_clocking(SP-program,false,false)";
const AUDIO: &str = "send_audio(42,sr=48000,ch=2,spc=1600)";
const UYVY: &str = "send_video_async(42,UYVY,4x2,stride=8,30/1)";
const NV12: &str = "send_video_async(42,NV12,4x2,stride=4,30/1)";

/// The sender on the 4×2 canvas, with VBAN and MAX's hand-off, NDI's source
/// set to `setting`.
struct Rig {
    backend: Arc<MockNdiBackend>,
    out: ProgramOutput<MockNdiBackend>,
    max: Arc<MaxOut>,
    vban: Arc<VbanOut>,
}

fn rig(setting: NdiSource) -> Rig {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let max = Arc::new(MaxOut::new());
    max.set_ndi_source(setting);
    let vban = Arc::new(VbanOut::new());
    let out = ProgramOutput::new(sender, W, H)
        .with_max(max.clone())
        .with_vban(vban.clone());
    Rig {
        backend,
        out,
        max,
        vban,
    }
}

/// A whole 4×2 UYVY picture, every byte `v`.
fn uyvy(v: u8) -> SharedFrame {
    SharedFrame::new(vec![v; (W * 2 * H) as usize])
}

/// A forwarded 4×2 pair on boundary `k`: its audio `level` (+ a step, so
/// left and right differ), stamped 77 after the boundary.
fn source(k: usize, level: f32) -> ProgramJob {
    ProgramJob::Source(pair(W, at(k), at(k) + 77, samples(level, 1e-5)))
}

/// The picture NDI got last.
fn wire(rig: &Rig) -> SharedFrame {
    rig.out.wire_frame().cloned().expect("a picture went out")
}

/// The calls after the sender's creation.
fn sends(rig: &Rig) -> Vec<String> {
    rig.backend.calls()[1..].to_vec()
}

/// The next block VBAN got.
fn vban_block(rig: &Rig) -> ProgramBlock {
    match rig.vban.take_timeout(Duration::ZERO) {
        VbanTake::Block(block) => block,
        other => panic!("VBAN got no block: {other:?}"),
    }
}

#[test]
fn from_the_gpu_each_boundary_goes_out_one_boundary_late_with_its_audio_and_stamps() {
    let mut rig = rig(NdiSource::Gpu);
    let picture = uyvy(1);
    rig.max.ndi.push(at(0), picture.clone());

    rig.out.submit(source(0, 0.25));
    assert_eq!(
        rig.backend.calls(),
        [CREATE],
        "the switch holds b0: no send"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (status.source, status.switches, status.repeats),
        ("gpu", 1, 0),
        "{status:?}"
    );
    assert_eq!(
        vban_block(&rig),
        ProgramBlock {
            due_100ns: at(0),
            samples: Some(samples(0.25, 1e-5).into()),
            substituted: false,
        },
        "VBAN has b0 at b0"
    );

    rig.out.submit(source(1, 0.5));
    assert_eq!(sends(&rig), [AUDIO, UYVY], "b0's pair, audio first, UYVY");
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::UYVY)
    );
    assert!(wire(&rig).ptr_eq(&picture), "b0's readback, no copy");
    assert_eq!(rig.backend.video_timecodes(), [at(0)], "b0's stamp");
    assert_eq!(
        rig.backend.audio_timecodes(),
        [at(0) + 77],
        "b0's audio stamp"
    );
    assert_eq!(
        interleaved(&rig.backend.last_audio_planar()),
        samples(0.25, 1e-5),
        "b0's audio, after the limiter (at rest: bit for bit)"
    );
    assert_eq!(vban_block(&rig).due_100ns, at(1), "VBAN has b1 at b1");
    let status = rig.max.ndi_status();
    assert_eq!(
        (
            status.delay_slots,
            status.fourcc,
            status.switches,
            status.repeats
        ),
        (1, "UYVY", 1, 0),
        "one switch, then steady: {status:?}"
    );
}

#[test]
fn a_missing_picture_repeats_the_last_one_and_the_black_before_any() {
    let mut rig = rig(NdiSource::Gpu);
    let p1 = uyvy(1);
    // Fresh for b0, but nothing of b0 itself.
    rig.max.ndi.push(at(1), p1.clone());
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    let black = wire(&rig);
    assert_eq!(
        black[..],
        UYVY_BLACK_PAIR.repeat((W / 2 * H) as usize)[..],
        "b0's picture never came: the constant black"
    );
    assert_eq!(rig.max.ndi_status().repeats, 1);

    rig.out.submit(source(2, 0.25));
    assert!(wire(&rig).ptr_eq(&p1), "b1's own readback");
    assert_eq!(rig.max.ndi_status().repeats, 1, "not a repeat");

    rig.out.submit(source(3, 0.25));
    assert!(wire(&rig).ptr_eq(&p1), "b2's never came: b1's again");
    assert_eq!(rig.max.ndi_status().repeats, 2);

    // A picture short of whole UYVY for the canvas is never sent.
    rig.max.ndi.push(at(3), SharedFrame::new(vec![9; 15]));
    rig.out.submit(source(4, 0.25));
    assert!(wire(&rig).ptr_eq(&p1), "not whole: b1's again");
    assert_eq!(rig.max.ndi_status().repeats, 3);
    // An exactly whole one is.
    let p4 = uyvy(4);
    rig.max.ndi.push(at(4), p4.clone());
    rig.out.submit(source(5, 0.25));
    assert!(wire(&rig).ptr_eq(&p4));
    assert_eq!(rig.max.ndi_status().repeats, 3);
    assert_eq!(
        rig.backend.video_timecodes(),
        [at(0), at(1), at(2), at(3), at(4)],
        "every boundary went out once, one late"
    );
}

/// #279 ROZHODNUTÉ 6232669201 item 1: a switch back to the canvas never
/// drops an audio block. The pair held from the GPU goes out FIRST (its
/// readback, else the last picture, else the black: a repeat), then the
/// boundary on the canvas: two submits, contiguous stamps.
#[test]
fn a_switch_to_the_gpu_holds_one_boundary_and_back_sends_the_held_pair_first() {
    let mut rig = rig(NdiSource::Cpu);
    rig.out.submit(source(0, 0.25));
    assert_eq!(sends(&rig), [AUDIO, NV12], "b0 on the canvas");

    rig.max.set_ndi_source(NdiSource::Gpu);
    let p1 = uyvy(1);
    rig.max.ndi.push(at(0), uyvy(0));
    rig.out.submit(source(1, 0.25));
    assert_eq!(sends(&rig).len(), 2, "b1 held: nothing sent");
    rig.max.ndi.push(at(1), p1.clone());
    rig.out.submit(source(2, 0.5));
    assert!(wire(&rig).ptr_eq(&p1), "b1 from the GPU");

    // Back on the canvas: b2, held, goes out first (its picture never
    // came: b1's again, a repeat), then b3 on the canvas.
    rig.max.set_ndi_source(NdiSource::Cpu);
    rig.out.submit(source(3, 0.75));
    assert_eq!(
        sends(&rig),
        [AUDIO, NV12, AUDIO, UYVY, AUDIO, UYVY, AUDIO, NV12],
        "b0 canvas, b1 + b2 from the GPU, b3 canvas"
    );
    assert_eq!(
        rig.backend.audio_timecodes(),
        [at(0) + 77, at(1) + 77, at(2) + 77, at(3) + 77],
        "every boundary's audio block went out once, b2's included"
    );
    assert_eq!(
        interleaved(&rig.backend.last_audio_planar()),
        samples(0.75, 1e-5),
        "b3's own block last"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (
            status.source,
            status.fourcc,
            status.delay_slots,
            status.switches,
            status.repeats
        ),
        ("cpu", "NV12", 0, 2, 1),
        "{status:?}"
    );

    // Back to the GPU (the setting's flip: at once): b4 is held, and b4's
    // picture never came: the black, not b1's picture from before the
    // setting's switch.
    rig.max.set_ndi_source(NdiSource::Gpu);
    rig.out.submit(source(4, 0.25));
    rig.out.submit(source(5, 0.25));
    assert_eq!(
        wire(&rig)[..],
        UYVY_BLACK_PAIR.repeat((W / 2 * H) as usize)[..]
    );
    assert_eq!(
        rig.backend.video_timecodes(),
        [at(0), at(1), at(2), at(3), at(4)],
        "contiguous, b4 one late"
    );
    let status = rig.max.ndi_status();
    assert_eq!((status.switches, status.repeats), (3, 2), "{status:?}");
}

/// A readback that only ever arrives after its boundary went out (every
/// picture late): the feed keeps nothing fresh, so 31 slots after the last
/// picture it kept NDI falls back to the canvas, never repeating one
/// picture for good. #279 ROZHODNUTÉ 6232669201 item 3: while the readback
/// stays late NDI stays on the canvas (no flapping back for one boundary),
/// and once it is on time again it goes back to the GPU only after 30
/// on-time readbacks in a row. A picture that did not come then repeats
/// the boundary before's readback, which the gate took on the returning
/// boundary (every canvas boundary forgets the last picture): never a
/// black frame.
#[test]
fn a_readback_that_is_always_late_falls_back_and_stays_on_the_canvas() {
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    for k in 2..=30 {
        rig.out.submit(source(k, 0.25));
        // Boundary k − 1 went out (a repeat) before its picture came.
        rig.max.ndi.push(at(k - 1), uyvy(2));
        assert_eq!(
            rig.backend.last_async_video_fourcc(),
            Some(FourCCVideoType::UYVY),
            "boundary {k}: from the GPU"
        );
    }
    rig.out.submit(source(31, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12),
        "31 slots after the last picture kept"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (
            status.source,
            status.repeats,
            status.readback_late,
            status.switches
        ),
        ("cpu", 30, 29, 2),
        "b30, held, went out first (b0's picture again): {status:?}"
    );
    // Still late: NDI stays on the canvas, it never flaps back.
    for k in 32..=71 {
        rig.out.submit(source(k, 0.25));
        rig.max.ndi.push(at(k - 1), uyvy(2));
        assert_eq!(
            rig.backend.last_async_video_fourcc(),
            Some(FourCCVideoType::NV12),
            "boundary {k}: the late readback never brings NDI back"
        );
    }
    let status = rig.max.ndi_status();
    assert_eq!(
        (status.switches, status.readback_late),
        (2, 69),
        "{status:?}"
    );
    // On time again: the canvas until 30 in a row came.
    for k in 72..=100 {
        rig.max.ndi.push(at(k - 1), uyvy(3));
        rig.out.submit(source(k, 0.25));
        assert_eq!(
            rig.backend.last_async_video_fourcc(),
            Some(FourCCVideoType::NV12),
            "boundary {k}: {} on-time readbacks in a row",
            k - 71
        );
    }
    let p100 = uyvy(4);
    rig.max.ndi.push(at(100), p100.clone());
    rig.out.submit(source(101, 0.25));
    assert_eq!(
        rig.backend.video_timecodes().last(),
        Some(&at(100)),
        "the 30th on-time readback: back on the GPU, b101 held"
    );
    assert_eq!(rig.max.ndi_status().switches, 3);
    // b101's own picture did not come: the last read-back one, b100's.
    rig.out.submit(source(102, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::UYVY)
    );
    assert!(
        wire(&rig).ptr_eq(&p100),
        "the last read-back picture, not the black"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (status.source, status.switches, status.repeats),
        ("gpu", 3, 31),
        "{status:?}"
    );
}

#[test]
fn a_stale_readback_falls_back_to_the_canvas() {
    let mut rig = rig(NdiSource::Gpu);
    let p0 = uyvy(1);
    rig.max.ndi.push(at(0), p0.clone());
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    assert!(wire(&rig).ptr_eq(&p0));
    // The readback stops. 30 slots after its newest picture NDI still
    // comes from it (b1 repeats b0's picture)...
    rig.out.submit(source(30, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::UYVY)
    );
    assert!(wire(&rig).ptr_eq(&p0), "b1 repeats b0's picture");
    // ...31 slots after it, the canvas: b30, held, goes out first (b0's
    // picture again, a repeat), then b31 on the canvas.
    rig.out.submit(source(31, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12)
    );
    assert_eq!(
        rig.backend.video_timecodes(),
        [at(0), at(1), at(30), at(31)]
    );
    assert_eq!(
        rig.backend.audio_timecodes(),
        [at(0) + 77, at(1) + 77, at(30) + 77, at(31) + 77],
        "no audio block lost at the fallback"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (status.source, status.switches, status.repeats),
        ("cpu", 2, 2),
        "{status:?}"
    );
}

/// #279 ROZHODNUTÉ 6232669201 item 3: after an automatic fallback the
/// on-time readbacks are counted in a row, so a late (or missing) one in
/// the run starts the count again.
#[test]
fn after_a_fallback_a_missing_readback_starts_the_count_again() {
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    rig.out.submit(source(32, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12),
        "32 slots after b0's picture: the canvas"
    );
    for k in 33..=61 {
        rig.max.ndi.push(at(k - 1), uyvy(2));
        rig.out.submit(source(k, 0.25));
    }
    // b61's picture never comes: 29 in a row, then the count starts again.
    rig.out.submit(source(62, 0.25));
    for k in 63..=91 {
        rig.max.ndi.push(at(k - 1), uyvy(2));
        rig.out.submit(source(k, 0.25));
        assert_eq!(
            rig.backend.last_async_video_fourcc(),
            Some(FourCCVideoType::NV12),
            "boundary {k}: {} in a row since the missing one",
            k - 62
        );
    }
    assert_eq!(rig.max.ndi_status().switches, 2);
    rig.max.ndi.push(at(91), uyvy(2));
    rig.out.submit(source(92, 0.25));
    assert_eq!(
        rig.backend.video_timecodes().last(),
        Some(&at(91)),
        "30 in a row: back on the GPU, b92 held"
    );
    let status = rig.max.ndi_status();
    assert_eq!((status.source, status.switches), ("gpu", 3), "{status:?}");
}

/// The first switch to the GPU (no fallback before it) needs no run of
/// on-time readbacks: NDI waits on the canvas only for the first picture,
/// then goes to the GPU at once. The gate holds only after a fallback.
#[test]
fn the_first_switch_to_the_gpu_needs_no_run_of_on_time_readbacks() {
    let mut rig = rig(NdiSource::Gpu);
    rig.out.submit(source(0, 0.25));
    assert_eq!(
        sends(&rig),
        [AUDIO, NV12],
        "no picture read back yet: the canvas"
    );
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(1, 0.25));
    assert_eq!(sends(&rig).len(), 2, "b1 held: on the GPU at once");
    let status = rig.max.ndi_status();
    assert_eq!((status.source, status.switches), ("gpu", 1), "{status:?}");
}

/// The first switch to the GPU takes the boundary before's readback as the
/// last picture (review round 3): a picture missing right after the switch
/// repeats that boundary, never a black frame.
#[test]
fn the_first_switch_to_the_gpu_repeats_the_boundary_before_never_the_black() {
    let mut rig = rig(NdiSource::Gpu);
    rig.out.submit(source(0, 0.25));
    let p0 = uyvy(1);
    rig.max.ndi.push(at(0), p0.clone());
    rig.out.submit(source(1, 0.25));
    rig.out.submit(source(2, 0.25));
    assert!(
        wire(&rig).ptr_eq(&p0),
        "b1's readback never came: b0's picture, not the black"
    );
    let status = rig.max.ndi_status();
    assert_eq!((status.switches, status.repeats), (1, 1), "{status:?}");
}

/// At the setting's switch to the canvas the held pair goes out with the
/// last read-back picture (a repeat) BEFORE the switch forgets it: read
/// inside b2's own submit, b1's picture on the wire is b0's, never the
/// black.
#[test]
fn the_setting_s_switch_sends_the_held_pair_before_it_forgets_the_last_picture() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let backend = Arc::new(HookedNdi {
        inner: MockNdiBackend::new(),
        on_audio: Box::new(move |mock: &MockNdiBackend| {
            log.lock().unwrap().push(mock.last_async_video_slice());
        }),
    });
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let max = Arc::new(MaxOut::new());
    max.set_ndi_source(NdiSource::Gpu);
    let mut out = ProgramOutput::new(sender, W, H).with_max(max.clone());
    let p0 = uyvy(1);
    max.ndi.push(at(0), p0.clone());
    out.submit(source(0, 0.25));
    out.submit(source(1, 0.25));
    max.set_ndi_source(NdiSource::Cpu);
    out.submit(source(2, 0.25));
    let p0_slice = Some((p0.as_ptr() as usize, p0.len()));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [None, p0_slice, p0_slice],
        "the pictures on the wire at b0's, b1's and b2's audio"
    );
    assert_eq!(
        backend.inner.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12),
        "b2 on the canvas"
    );
}

/// At a switch to the canvas the video side starts before the held pair's
/// submit, so `health.timing.submit_us` covers both submits.
#[test]
fn a_switch_to_the_canvas_times_both_submits() {
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.max.set_ndi_source(NdiSource::Cpu);
    let reads = Cell::new(0_i64);
    let now = || {
        reads.set(reads.get() + 1);
        reads.get()
    };
    let marks = rig.out.serve(source(1, 0.25), None, now);
    assert_eq!(
        (
            marks.taken_100ns,
            marks.fed_100ns,
            marks.submit_start_100ns,
            marks.submitted_100ns
        ),
        (1, 2, 3, 5),
        "the start read before b0's submit (3), not the canvas picture's own (4)"
    );
}

/// Under the setting's canvas this output lets the feed's pictures go,
/// AFTER the held pair's flush took its own (review rounds 4-5: the flush
/// first, so it repeats nothing), and at every such boundary, not only the
/// switch: the `program-max` thread no longer lets them go, and it can
/// push one more picture after the switch (it read the setting late).
#[test]
fn the_setting_s_switch_lets_the_feed_s_pictures_go() {
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.max.ndi.push(at(1), uyvy(2));
    rig.max.ndi.push(at(2), uyvy(3));
    rig.max.set_ndi_source(NdiSource::Cpu);
    rig.out.submit(source(1, 0.25));
    assert_eq!(
        rig.backend.video_timecodes(),
        [at(0), at(1)],
        "b0 flushed with its picture, b1 on the canvas"
    );
    assert_eq!(
        rig.max.ndi_status().repeats,
        0,
        "the flush took b0's picture before the feed let it go"
    );
    assert!(rig.max.ndi.take(at(2)).is_none(), "the feed let it go");
    // The thread read the setting late: one more picture after the switch.
    rig.max.ndi.push(at(3), uyvy(4));
    rig.out.submit(source(2, 0.25));
    assert!(
        rig.max.ndi.take(at(3)).is_none(),
        "the next canvas boundary let it go too"
    );
}

/// Every canvas boundary forgets the last read-back picture: after a
/// fallback nothing holds it (review round 4), so its 4 MB go back to the
/// frame pool. A size class no other test uses.
#[test]
fn a_fallback_to_the_canvas_lets_the_last_picture_go() {
    const CAP: usize = 2_311_219;
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), SharedFrame::new(vec![1; CAP]));
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    let before = sp_decoder::frame_pool::pool_len(CAP);
    rig.out.submit(source(31, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12),
        "the stale fallback: b1 out first, then b31 on the canvas"
    );
    assert_eq!(
        sp_decoder::frame_pool::pool_len(CAP),
        before + 1,
        "nothing holds the last picture any more"
    );
}

/// The setting's switch to the canvas ends a fallback's count: a later
/// flip back to `gpu` goes to the GPU once the newest picture is fresh, as the
/// first switch does.
#[test]
fn the_setting_s_canvas_ends_a_fallback_and_its_flip_back_is_at_once() {
    let mut rig = rig(NdiSource::Gpu);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    rig.out.submit(source(32, 0.25));
    assert_eq!(rig.max.ndi_status().switches, 2, "the stale fallback");
    rig.max.set_ndi_source(NdiSource::Cpu);
    rig.out.submit(source(33, 0.25));
    rig.max.set_ndi_source(NdiSource::Gpu);
    rig.max.ndi.push(at(33), uyvy(2));
    rig.out.submit(source(34, 0.25));
    assert_eq!(
        rig.backend.video_timecodes().last(),
        Some(&at(33)),
        "b34 held: on the GPU at once"
    );
    let status = rig.max.ndi_status();
    assert_eq!((status.source, status.switches), ("gpu", 3), "{status:?}");
}

/// A fade boundary from the GPU: its audio crossfaded as on the canvas,
/// its picture the compositor's (no canvas picture, no fit, no band-pool
/// work), counted in the run of mixed boundaries when it goes out.
#[test]
fn a_fade_from_the_gpu_crossfades_its_audio_and_sends_the_compositor_s_picture() {
    let mut rig = rig(NdiSource::Gpu);
    let mix = |k: usize, slot: u32| {
        // An 8×2 outgoing side: on the canvas it would be fitted.
        ProgramJob::Mix(MixJob {
            stamp_100ns: at(k),
            from: Some(pair(8, at(k), at(k) + 11, samples(0.25, 0.0))),
            to: Some(pair(W, at(k), at(k) + 22, samples(0.5, 0.0))),
            slot,
            n_slots: 9,
        })
    };
    let p0 = uyvy(1);
    rig.max.ndi.push(at(0), p0.clone());
    rig.out.submit(mix(0, 4));
    rig.out.submit(mix(1, 5));
    assert!(
        wire(&rig).ptr_eq(&p0),
        "the compositor's fade, as read back"
    );
    let vban = vban_block(&rig);
    assert_eq!(vban.due_100ns, at(0));
    let block = vban.samples.expect("the crossfaded block");
    let (g_from, g_to) = crossfade_gains(4 * 1600, 9 * 1600);
    assert_eq!(block[0], g_from * 0.25 + g_to * 0.5, "slot 4 of 9");
    assert_eq!(
        interleaved(&rig.backend.last_audio_planar())[..],
        block[..],
        "NDI carries VBAN's crossfaded block, one boundary late"
    );
    assert_eq!(
        (rig.backend.video_timecodes(), rig.backend.audio_timecodes()),
        (vec![at(0)], vec![at(0)]),
        "the program's own block, stamped on its boundary"
    );
    assert_eq!(
        (
            rig.out.mix_run.boundaries,
            rig.out.mix_run.fitted,
            rig.out.mix_run.max_picture_us
        ),
        (1, 0, 0),
        "one mixed boundary went out, no canvas picture made"
    );
    assert_eq!(rig.out.fit_plans(), 0, "nothing fitted");
    rig.out.submit(ProgramJob::Standby { stamp_100ns: at(2) });
    assert_eq!(rig.out.mix_run, MixRun::default(), "the run ended with b1");
    assert_eq!(rig.backend.video_timecodes(), [at(0), at(1)]);
}

/// The burn of the item frame a held pair shows is painted on a COPY of
/// its read-back UYVY picture (`burned_uyvy`), with that boundary's frame
/// and wire stamp, when the pair goes out one boundary late. The
/// production canvas (1920×1080): the burn needs its 270 rows.
#[test]
fn the_burn_on_the_readback_is_painted_into_a_copy_of_the_uyvy_picture() {
    const PID: i64 = 5;
    let backend = Arc::new(MockNdiBackend::new());
    let sender =
        NdiSender::new_with_clocking(backend, PROGRAM_NDI_NAME, false, false).expect("mock sender");
    let max = Arc::new(MaxOut::new());
    max.set_ndi_source(NdiSource::Gpu);
    let item = Arc::new(ProgramItem::default());
    item.mark(PID, 42, 0);
    item.set_burn(true);
    let mut out = ProgramOutput::fhd(sender)
        .with_max(max.clone())
        .with_item(item.clone());
    // A 1920×1080 UYVY picture: luma 255, chroma 128.
    let white = [128u8, 255, 128, 255].repeat(960 * 1080);
    let picture = SharedFrame::new(white.clone());
    max.ndi.push(at(1), picture.clone());
    // Frame 1 of the item (33 ms after its mark), on boundary 1.
    let job = SubmitJob {
        media_pts_100ns: Some(330_000),
        live: true,
        ..pair(W, at(1), at(1), samples(0.25, 0.0))
    };
    out.serve(ProgramJob::Source(job), Some(PID), || 0);
    assert_eq!(item.burned(), 0, "held: nothing went out");
    out.serve(ProgramJob::Standby { stamp_100ns: at(2) }, Some(PID), || 0);
    let sent = out.wire_frame().cloned().expect("boundary 1 went out");
    assert!(!sent.ptr_eq(&picture), "painted into a copy");
    assert!(
        picture[..] == white[..],
        "the read-back picture is untouched"
    );
    let gen_ts_ns = fleet_shift::wire_100ns(at(1)) * 100;
    let want = program_burn::burned_uyvy(&white, 1920, 1080, 1, gen_ts_ns).expect("room");
    assert!(
        sent[..] == want[..],
        "frame 1's burn, on boundary 1's wire stamp"
    );
    assert_eq!(item.burned(), 1);
}

#[test]
fn the_trace_names_the_stamp_that_went_out() {
    let mut rig = rig(NdiSource::Gpu);
    let trace = Arc::new(ProgramTrace::with_capacity(8));
    rig.out = rig.out.with_trace(&trace);
    rig.max.ndi.push(at(0), uyvy(1));
    rig.out.submit(source(0, 0.25));
    rig.out.submit(source(1, 0.25));
    let k = fleet_shift::global().slots();
    let records = trace.records_from(0);
    assert_eq!(
        records
            .iter()
            .map(|r| (r.stamp_100ns, r.wire_100ns))
            .collect::<Vec<_>>(),
        [
            (at(0), fleet_shift::wire_stamp_100ns(at(0), k)),
            (at(1), fleet_shift::wire_stamp_100ns(at(0), k)),
        ],
        "b0 sent nothing (its own stamp), b1 sent b0's"
    );
}
