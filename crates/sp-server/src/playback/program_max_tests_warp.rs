//! #223 S2 end to end on WARP (Windows only, `windows-latest` has no GPU): a
//! `ProgramOutput` (mock NDI) offers its boundaries to a real `program-max`
//! thread whose GPU is `sp-gpu`'s compositor on WARP and a Spout sender
//! under a test name. One Source boundary, then a Mix boundary: Spout's
//! registry lists the sender at 3840×2160, and its shared texture, opened
//! and read on a SECOND WARP device as Arena opens it, shows the fitted
//! picture and then the fade within S1a's tolerance (`reference.rs`: one
//! code per quad that covers a pixel). #239: the same thread runs the FHD
//! sender too (#279 P1b: the WARP compositor's PRIMARY target is the
//! 1920×1080 program, MAX's 3840×2160 target is added to it with MAX's
//! sender; one compose draws both; each sender under its own test name):
//! Spout lists it at 1920×1080 (the thread's own registry read,
//! `max.fhd.listed_*`, says so too) and its texture holds the same pictures
//! fitted into 1920×1080. #267: a third, 4:3 boundary fills MAX's whole
//! width (no black column at either side), while the FHD sender keeps the
//! NDI `SP-program`'s bars. #279 P1b: then NDI `SP-program` from the GPU
//! readback ([`check_ndi_from_the_readback`]): a 1920×1080 `ProgramOutput`
//! in gpu mode sends each boundary one boundary late as UYVY, the FHD
//! target's own picture, for a Source boundary and a fade.
//!
//! The readback takes no Spout mutex, so each picture is served until both
//! senders sent it TWICE (the run's first picture one boundary more: the
//! FHD output is built after MAX's first send) and read after that: the
//! last draw waited for the GPU, which ran the earlier send's copy before
//! it, and the last copy writes the same bytes. Spout's list is
//! machine-wide; this is the only Spout test in the sp-server binary, and
//! cargo runs test binaries one at a time.
//! Wired via `#[cfg(test)] #[cfg(windows)] #[path = "program_max_tests_warp.rs"] mod tests_warp;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::config::NdiSource;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_gpu::uyvy::{UYVY_TOLERANCE, reference as uyvy_reference};
use sp_gpu::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Compositor, FHD_FIT, FHD_HEIGHT, FHD_WIDTH, GpuError,
    Layer, Nv12Picture, Side, SpoutSender, TargetId, read_shared_texture, reference,
    spout_sender_info,
};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, FourCCVideoType, NdiSender};

use super::MaxOut;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_max_worker::{MaxGpu, run_max_loop};
use crate::playback::program_output::ProgramOutput;
use crate::playback::program_transition::{MixJob, weight_q8};
use crate::playback::submit_handoff::SubmitJob;

/// The sender's name here: never the production `SP-program-MAX`.
const TEST_SENDER: &str = "SP-program-MAX-s2-warp-test";

/// #239: the FHD sender's name here: never the production `SP-program`.
const TEST_FHD_SENDER: &str = "SP-program-fhd-warp-test";

/// The WARP GPU, the production topology (#279 P1b): a compositor whose
/// primary target is the 1920×1080 program (`new_warp_with_size`, FHD's
/// fit), the sender under [`TEST_SENDER`] of the MAX target the worker adds
/// to it (the production `MaxCompositor::add_target` of
/// `sp_gpu::Compositor`), and the sender under [`TEST_FHD_SENDER`] of the
/// primary.
struct WarpGpu;

impl MaxGpu for WarpGpu {
    type Compositor = Compositor;
    type Sender = SpoutSender;

    fn compositor(&mut self) -> Result<Compositor, GpuError> {
        Compositor::new_warp_with_size(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)
    }

    fn sender(
        &mut self,
        compositor: &Compositor,
        target: TargetId,
    ) -> Result<SpoutSender, GpuError> {
        SpoutSender::for_target(compositor, target, TEST_SENDER)
    }

    fn fhd_sender(
        &mut self,
        compositor: &Compositor,
        target: TargetId,
    ) -> Result<SpoutSender, GpuError> {
        SpoutSender::for_target(compositor, target, TEST_FHD_SENDER)
    }
}

/// `lo..=hi` up and down, one per step of `t` (S1a's smooth pattern).
fn triangle(t: u32, lo: u8, hi: u8) -> u8 {
    let span = u32::from(hi - lo);
    let phase = t % (2 * span);
    let up = if phase <= span {
        phase
    } else {
        2 * span - phase
    };
    lo + up as u8
}

/// A smooth `width`×`height` NV12 picture (≤ 12 luma codes per texel
/// across, ≤ 5 down, ≤ 8 chroma codes per chroma texel: S1a's tolerance
/// needs smooth pictures), rows padded by 64 bytes of 0xFF.
fn smooth(width: u32, height: u32, seed: u32) -> (u32, Vec<u8>) {
    let stride = width + 64;
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut data = vec![0xFF_u8; (stride * (height + ch)) as usize];
    for y in 0..height {
        for x in 0..width {
            data[(y * stride + x) as usize] = triangle(12 * x + 5 * y + seed, 16, 235);
        }
    }
    let chroma = (stride * height) as usize;
    for y in 0..ch {
        for x in 0..cw {
            let at = chroma + (y * stride + 2 * x) as usize;
            data[at] = triangle(8 * x + 3 * y + 2 * seed + 40, 16, 240);
            data[at + 1] = triangle(6 * x + 7 * y + 3 * seed + 90, 16, 240);
        }
    }
    (stride, data)
}

/// A source boundary carrying a smooth picture and one silent block.
fn source(width: u32, height: u32, seed: u32, stamp: i64) -> SubmitJob {
    let (stride, data) = smooth(width, height, seed);
    SubmitJob {
        width,
        height,
        stride,
        video: SharedFrame::new(data).into(),
        audio: vec![AudioFrame {
            data: vec![0.0; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
        media_pts_100ns: None,
        blend: None,
    }
}

/// `job`'s picture as an uploaded side, for the CPU reference.
fn nv12(job: &SubmitJob) -> Side<'_> {
    Side::Cpu(Nv12Picture {
        id: 0,
        width: job.width,
        height: job.height,
        stride: job.stride,
        data: job.video.expect_cpu(),
    })
}

/// The k-th grid boundary from now.
fn at(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(17_900_000_000_000_000, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The BGRA at (`x`, `y`) of a frame `width` pixels wide.
fn bgra(frame: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

/// Compare a `(w, h)` `frame` with the CPU reference of `layers` on a grid
/// of points (every 7th column, every 9th row, the corners, and both sides
/// of every quad edge): each colour within `reference::tolerance`, alpha
/// exact.
fn assert_matches(frame: &[u8], layers: &[Layer<'_>], (w, h): (u32, u32), what: &str) {
    assert_eq!(frame.len(), (w * h * 4) as usize, "{what}: frame size");
    let mut points: Vec<(u32, u32)> = (0..h)
        .step_by(9)
        .flat_map(|y| (0..w).step_by(7).map(move |x| (x, y)))
        .collect();
    points.extend([(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]);
    for layer in layers {
        let p = layer.place;
        for y in [
            p.off_y.wrapping_sub(1),
            p.off_y,
            p.off_y + p.h - 1,
            p.off_y + p.h,
        ] {
            if y < h {
                points.extend((0..w).step_by(3).map(|x| (x, y)));
            }
        }
        for x in [
            p.off_x.wrapping_sub(1),
            p.off_x,
            p.off_x + p.w - 1,
            p.off_x + p.w,
        ] {
            if x < w {
                points.extend((0..h).step_by(3).map(|y| (x, y)));
            }
        }
    }
    let mut lit = 0;
    let over: Vec<String> = points
        .iter()
        .filter_map(|&(x, y)| {
            let got = bgra(frame, w, x, y);
            let want = reference::pixel(layers, x, y);
            let tolerance = reference::tolerance(layers, x, y);
            if want[..3] != [0, 0, 0] {
                lit += 1;
            }
            let diff = (0..3).map(|c| got[c].abs_diff(want[c])).max().unwrap_or(0);
            (diff > tolerance || got[3] != want[3])
                .then(|| format!("({x}, {y}): got {got:?}, want {want:?} within {tolerance}"))
        })
        .collect();
    assert!(lit > 1000, "{what}: the reference has a picture to compare");
    assert!(
        over.is_empty(),
        "{what}: {} of {} points outside the tolerance, first: {:?}",
        over.len(),
        points.len(),
        &over[..over.len().min(10)]
    );
}

/// Serve `job` from boundary `*next` on until the FHD sender sent two of
/// its boundaries (and MAX at least as many), each boundary once the one
/// before went out (so the 2-deep queue never drops one). The run's first
/// boundary goes out on MAX alone: the FHD output is built after MAX's
/// send and goes out from the next boundary on (#239: MAX first, whole).
fn serve_twice(
    out: &mut ProgramOutput<MockNdiBackend>,
    max: &MaxOut,
    job: impl Fn(i64) -> ProgramJob,
    next: &mut usize,
) {
    let start = max.status();
    loop {
        let before = max.status();
        let k = *next;
        out.submit(job(at(k)));
        *next += 1;
        wait_until("the boundary went out", || {
            let now = max.status();
            now.submitted > before.submitted && (k == 0 || now.fhd.submitted > before.fhd.submitted)
        });
        if max.status().fhd.submitted >= start.fhd.submitted + 2 {
            break;
        }
    }
    let status = max.status();
    assert_eq!((status.failed, status.fhd.failed), (0, 0), "{status:?}");
}

/// The gpu-mode `SP-program` sender of [`check_ndi_from_the_readback`], kept
/// across its calls (its delay line holds a pair from one to the next): a
/// 1920×1080 `ProgramOutput` on the mock NDI, offering to `max`.
struct NdiRig {
    backend: Arc<MockNdiBackend>,
    out: ProgramOutput<MockNdiBackend>,
}

impl NdiRig {
    fn new(max: &Arc<MaxOut>) -> Self {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
            .expect("mock sender");
        let out = ProgramOutput::fhd(sender).with_max(max.clone());
        Self { backend, out }
    }
}

/// #279 P1b: switch NDI to the GPU readback and serve `job` on `rig` from
/// boundary `*next` on, each boundary once the one before was read back
/// AND sent on the FHD Spout sender (so its picture is in the feed and the
/// shared texture holds the same picture), three boundaries: the third
/// sends the second's pair (one boundary late). Its NDI picture is UYVY,
/// 1920×1080, stride 3840, and every byte is within `UYVY_TOLERANCE` of
/// `sp_gpu::uyvy::reference` of the FHD target's own BGRA, read through the
/// Spout FHD sender's texture (the same picture: every boundary served
/// shows `job`).
fn check_ndi_from_the_readback(
    max: &Arc<MaxOut>,
    rig: &mut NdiRig,
    job: impl Fn(i64) -> ProgramJob,
    what: &str,
    next: &mut usize,
) {
    max.set_ndi_source(NdiSource::Gpu);
    for _ in 0..3 {
        let before = max.status().fhd.submitted;
        let stamp = at(*next);
        rig.out.submit(job(stamp));
        *next += 1;
        wait_until("the boundary was read back and sent", || {
            max.ndi.newest_stamp() == Some(stamp) && max.status().fhd.submitted > before
        });
    }
    assert_eq!(
        rig.backend.last_async_video_fourcc(),
        Some(FourCCVideoType::UYVY),
        "{what}: NDI's picture is the readback"
    );
    let video = rig
        .backend
        .calls()
        .into_iter()
        .rev()
        .find(|call| call.starts_with("send_video_async("))
        .expect("a picture went out");
    assert!(
        video.ends_with(",UYVY,1920x1080,stride=3840,30/1)"),
        "{what}: {video}"
    );
    let wire = rig.out.wire_frame().expect("a picture went out").clone();
    let info = spout_sender_info(TEST_FHD_SENDER)
        .expect("Spout's registry reads")
        .expect("the FHD sender is listed");
    let bgra = read_shared_texture(&info).expect("a second WARP device reads it");
    let want = uyvy_reference(&bgra, FHD_WIDTH, FHD_HEIGHT);
    assert_eq!(wire.len(), want.len(), "{what}: 2 bytes a pixel");
    let over: Vec<(usize, u8, u8)> = wire
        .iter()
        .zip(&want)
        .enumerate()
        .filter(|(_, (got, want))| got.abs_diff(**want) > UYVY_TOLERANCE)
        .map(|(i, (got, want))| (i, *got, *want))
        .collect();
    assert!(
        over.is_empty(),
        "{what}: {} bytes outside the tolerance, first (index, got, want): {:?}",
        over.len(),
        &over[..over.len().min(10)]
    );
}

#[test]
fn the_program_output_composes_on_warp_and_shares_it_over_spout() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    max.set_fhd_enabled(true);
    let thread = {
        let max = max.clone();
        std::thread::Builder::new()
            .name("program-max".into())
            .spawn(move || run_max_loop(&max, WarpGpu, None, None))
            .expect("spawn program-max")
    };
    wait_until("program-max takes jobs", || max.accepting());

    let backend = Arc::new(MockNdiBackend::new());
    let sender =
        NdiSender::new_with_clocking(backend, PROGRAM_NDI_NAME, false, false).expect("mock sender");
    let mut out = ProgramOutput::new(sender, 2, 2).with_max(max.clone());

    // A 1280×720 source, fitted into 3840×2160 (upscaled, 16:9: no bars).
    let one = source(1280, 720, 0, at(0));
    let plain = |stamp: i64| {
        ProgramJob::Source(SubmitJob {
            video_tc_100ns: stamp,
            ..one.clone()
        })
    };
    let mut next = 0;
    serve_twice(&mut out, &max, plain, &mut next);
    assert_eq!(
        (max.status().submitted, next),
        (3, 3),
        "the first boundary on MAX alone"
    );
    let info = spout_sender_info(TEST_SENDER)
        .expect("Spout's registry reads")
        .expect("the sender is listed after its first send");
    assert_eq!(
        (info.width, info.height, info.format),
        (3840, 2160, 87),
        "SP-program-MAX's canvas, B8G8R8A8_UNORM"
    );
    let frame = read_shared_texture(&info).expect("a second WARP device reads Spout's texture");
    let size = (CANVAS_WIDTH, CANVAS_HEIGHT);
    let fhd_size = (FHD_WIDTH, FHD_HEIGHT);
    let plain = Composition::Picture(nv12(&one));
    assert_matches(&frame, &plain.layers(), size, "the Source boundary");
    let fhd_info = spout_sender_info(TEST_FHD_SENDER)
        .expect("Spout's registry reads")
        .expect("the FHD sender is listed after its first send");
    assert_eq!(
        (fhd_info.width, fhd_info.height, fhd_info.format),
        (1920, 1080, 87),
        "SP-program's 1920x1080, B8G8R8A8_UNORM"
    );
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.listed_width, fhd.listed_height),
        (1920, 1080),
        "the thread's own registry read"
    );
    let frame = read_shared_texture(&fhd_info).expect("a second WARP device reads it");
    let layers = plain.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT);
    assert_matches(
        &frame,
        &layers,
        fhd_size,
        "the Source boundary in 1920x1080",
    );

    // A fade from a 21:9 picture (bars) to the 720p one, slot 4 of 9.
    let wide = source(1280, 536, 7, at(2));
    let mix = |stamp: i64| {
        ProgramJob::Mix(MixJob {
            stamp_100ns: stamp,
            from: Some(SubmitJob {
                video_tc_100ns: stamp,
                ..wide.clone()
            }),
            to: Some(SubmitJob {
                video_tc_100ns: stamp,
                ..one.clone()
            }),
            slot: 4,
            n_slots: 9,
        })
    };
    serve_twice(&mut out, &max, mix, &mut next);
    assert_eq!(max.status().submitted, 5);
    let frame = read_shared_texture(&info).expect("a second WARP device reads Spout's texture");
    let fade = Composition::Fade {
        from: Some(nv12(&wide)),
        to: Some(nv12(&one)),
        weight_q8: weight_q8(4, 9),
    };
    assert_matches(&frame, &fade.layers(), size, "the Mix boundary");
    let frame = read_shared_texture(&fhd_info).expect("a second WARP device reads it");
    let layers = fade.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT);
    assert_matches(&frame, &layers, fhd_size, "the Mix boundary in 1920x1080");

    // #267: a 4:3 source (PP's `MO3ucp2mBnM` is 4:3). MAX, the wall's feed,
    // fills its whole width (rows cut off top and bottom): no black column
    // at either side. The FHD sender keeps the NDI `SP-program`'s bars.
    let four_three = source(960, 720, 21, at(4));
    let narrow = |stamp: i64| {
        ProgramJob::Source(SubmitJob {
            video_tc_100ns: stamp,
            ..four_three.clone()
        })
    };
    serve_twice(&mut out, &max, narrow, &mut next);
    assert_eq!(max.status().submitted, 7);
    let narrow = Composition::Picture(nv12(&four_three));
    let frame = read_shared_texture(&info).expect("a second WARP device reads Spout's texture");
    let layers = narrow.layers();
    assert_eq!((layers[0].place.off_x, layers[0].place.w), (0, 3840));
    assert_matches(&frame, &layers, size, "the 4:3 boundary");
    for y in 0..CANVAS_HEIGHT {
        for x in [0, CANVAS_WIDTH - 1] {
            let got = bgra(&frame, CANVAS_WIDTH, x, y);
            assert_ne!(
                got,
                [0, 0, 0, 255],
                "MAX's edge column {x} is black at row {y}"
            );
        }
    }
    let frame = read_shared_texture(&fhd_info).expect("a second WARP device reads it");
    let layers = narrow.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT);
    assert_eq!((layers[0].place.off_x, layers[0].place.w), (240, 1440));
    assert_matches(&frame, &layers, fhd_size, "the 4:3 boundary in 1920x1080");
    assert_eq!(
        bgra(&frame, FHD_WIDTH, 239, 540),
        [0, 0, 0, 255],
        "the left bar"
    );
    assert_eq!(
        bgra(&frame, FHD_WIDTH, 1680, 540),
        [0, 0, 0, 255],
        "the right bar"
    );

    // #279 P1b: NDI `SP-program` from the GPU readback, the 720p picture
    // and then the fade.
    let source = |stamp: i64| {
        ProgramJob::Source(SubmitJob {
            video_tc_100ns: stamp,
            ..one.clone()
        })
    };
    let mut rig = NdiRig::new(&max);
    check_ndi_from_the_readback(&max, &mut rig, source, "a Source boundary", &mut next);
    check_ndi_from_the_readback(&max, &mut rig, mix, "a fade boundary", &mut next);
    let ndi = max.ndi_status();
    assert_eq!(
        (ndi.source, ndi.fourcc, ndi.delay_slots, ndi.switches),
        ("gpu", "UYVY", 1, 1),
        "{ndi:?}"
    );
    assert_eq!(
        (ndi.repeats, ndi.readback_late, ndi.feed_coalesced),
        (0, 0, 0),
        "every boundary's picture came before it was sent: {ndi:?}"
    );

    max.stop();
    wait_until("program-max exits on stop", || thread.is_finished());
    thread.join().expect("program-max");
    assert_eq!(
        spout_sender_info(TEST_SENDER).expect("Spout's registry reads"),
        None,
        "the sender unregistered when the thread released it"
    );
    assert_eq!(
        spout_sender_info(TEST_FHD_SENDER).expect("Spout's registry reads"),
        None,
        "so did the FHD sender"
    );
}
