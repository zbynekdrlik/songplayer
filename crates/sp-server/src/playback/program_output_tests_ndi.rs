//! #279 P1b: NDI `SP-program` from the GPU readback, one boundary late —
//! the delay line (`program_output_ndi.rs`) over the mock NDI, with VBAN and
//! MAX's hand-off, the readback feed filled by the test (no `program-max`
//! thread: nothing takes MAX's offers). Boundary b's audio goes to VBAN at
//! b and to NDI at b + 1 with b's stamps and b's read-back picture; a
//! missing picture repeats the last one (the black before any); the
//! switches hold one boundary or skip one; a fade's audio is crossfaded
//! and its picture is the compositor's; the burn is painted on the UYVY
//! copy; the trace names the stamp that went out. (VBAN before any NDI
//! call in gpu mode: `program_output_tests_order.rs`.)
//! Wired via `#[cfg(test)] #[path = "program_output_tests_ndi.rs"] mod tests_ndi;`.

use std::sync::Arc;
use std::time::Duration;

use sp_core::config::NdiSource;
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{FourCCVideoType, NdiSender};

use super::tests_order::{at, interleaved, pair, samples};
use super::{MixRun, ProgramOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::fleet_shift;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::UYVY_BLACK_PAIR;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_max::MaxOut;
use crate::playback::program_trace::ProgramTrace;
use crate::playback::program_transition::{MixJob, crossfade_gains};
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

#[test]
fn a_switch_to_the_gpu_holds_one_boundary_and_back_skips_one() {
    let mut rig = rig(NdiSource::Cpu);
    rig.out.submit(source(0, 0.25));
    assert_eq!(sends(&rig), [AUDIO, NV12], "b0 on the canvas");

    rig.max.set_ndi_source(NdiSource::Gpu);
    let p1 = uyvy(1);
    rig.max.ndi.push(at(0), uyvy(0));
    rig.out.submit(source(1, 0.25));
    assert_eq!(sends(&rig).len(), 2, "b1 held: nothing sent");
    rig.max.ndi.push(at(1), p1.clone());
    rig.out.submit(source(2, 0.25));
    assert!(wire(&rig).ptr_eq(&p1), "b1 from the GPU");

    rig.max.set_ndi_source(NdiSource::Cpu);
    rig.out.submit(source(3, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12),
        "b3 on the canvas"
    );
    let status = rig.max.ndi_status();
    assert_eq!(
        (
            status.source,
            status.fourcc,
            status.delay_slots,
            status.switches
        ),
        ("cpu", "NV12", 0, 2),
        "{status:?}"
    );

    // Back to the GPU: b4 is held (b2, dropped at the switch, never goes
    // out), and b4's picture never came: the black, not b1's picture from
    // before the switch.
    rig.max.set_ndi_source(NdiSource::Gpu);
    rig.out.submit(source(4, 0.25));
    rig.out.submit(source(5, 0.25));
    assert_eq!(
        wire(&rig)[..],
        UYVY_BLACK_PAIR.repeat((W / 2 * H) as usize)[..]
    );
    assert_eq!(
        rig.backend.video_timecodes(),
        [at(0), at(1), at(3), at(4)],
        "b2 skipped, b4 one late"
    );
    let status = rig.max.ndi_status();
    assert_eq!((status.switches, status.repeats), (3, 1), "{status:?}");
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
    // ...31 slots after it, the canvas: b30, held, is dropped.
    rig.out.submit(source(31, 0.25));
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::NV12)
    );
    assert_eq!(rig.backend.video_timecodes(), [at(0), at(1), at(31)]);
    let status = rig.max.ndi_status();
    assert_eq!(
        (status.source, status.switches, status.repeats),
        ("cpu", 2, 1),
        "{status:?}"
    );
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
