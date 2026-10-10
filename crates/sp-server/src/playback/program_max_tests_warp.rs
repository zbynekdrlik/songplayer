//! #223 S2 end to end on WARP (Windows only, `windows-latest` has no GPU): a
//! `ProgramOutput` (mock NDI) offers its boundaries to a real `program-max`
//! thread whose GPU is `sp-gpu`'s compositor on WARP and a Spout sender
//! under a test name. One Source boundary, then a Mix boundary: Spout's
//! registry lists the sender at 3840×2160, and its shared texture, opened
//! and read on a SECOND WARP device as Arena opens it, shows the fitted
//! picture and then the fade within S1a's tolerance (`reference.rs`: one
//! code per quad that covers a pixel). #239: the same thread runs the FHD
//! sender too (a 1920×1080 WARP compositor and a sender under its own test
//! name): Spout lists it at 1920×1080 (the thread's own registry read,
//! `max.fhd.listed_*`, says so too) and its texture holds the same pictures
//! fitted into 1920×1080.
//!
//! The readback takes no Spout mutex, so each picture is served TWICE and
//! read after the second one went out: its draw waited for the GPU, which
//! ran the first send's copy before it, and the second copy writes the same
//! bytes. Spout's list is machine-wide; this is the only Spout test in the
//! sp-server binary, and cargo runs test binaries one at a time.
//! Wired via `#[cfg(test)] #[cfg(windows)] #[path = "program_max_tests_warp.rs"] mod tests_warp;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_gpu::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Compositor, FHD_HEIGHT, FHD_WIDTH, GpuError, Layer,
    Nv12Picture, SpoutSender, read_shared_texture, reference, spout_sender_info,
};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

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

/// The WARP GPU: `Compositor::new_warp` and the sender under [`TEST_SENDER`];
/// a 1920×1080 WARP compositor and the sender under [`TEST_FHD_SENDER`].
struct WarpGpu;

impl MaxGpu for WarpGpu {
    type Compositor = Compositor;
    type Sender = SpoutSender;

    fn compositor(&mut self) -> Result<Compositor, GpuError> {
        Compositor::new_warp()
    }

    fn sender(&mut self, compositor: &Compositor) -> Result<SpoutSender, GpuError> {
        SpoutSender::with_name(compositor, TEST_SENDER)
    }

    fn fhd_compositor(&mut self) -> Result<Compositor, GpuError> {
        Compositor::new_warp_with_size(FHD_WIDTH, FHD_HEIGHT)
    }

    fn fhd_sender(&mut self, compositor: &Compositor) -> Result<SpoutSender, GpuError> {
        SpoutSender::with_name(compositor, TEST_FHD_SENDER)
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
        video: SharedFrame::new(data),
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
    }
}

fn nv12(job: &SubmitJob) -> Nv12Picture<'_> {
    Nv12Picture {
        id: 0,
        width: job.width,
        height: job.height,
        stride: job.stride,
        data: &job.video,
    }
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

/// Serve `job` on boundaries `first` and `first + 1`, each once MAX and
/// the FHD sender sent the one before (so the 2-deep queue never drops one).
fn serve_twice(
    out: &mut ProgramOutput<MockNdiBackend>,
    max: &MaxOut,
    job: impl Fn(i64) -> ProgramJob,
    first: usize,
) {
    for k in [first, first + 1] {
        let before = max.status();
        out.submit(job(at(k)));
        wait_until("MAX and the FHD sender sent the boundary", || {
            let now = max.status();
            now.submitted > before.submitted && now.fhd.submitted > before.fhd.submitted
        });
    }
    let status = max.status();
    assert_eq!((status.failed, status.fhd.failed), (0, 0), "{status:?}");
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
            .spawn(move || run_max_loop(&max, WarpGpu, None))
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
    serve_twice(&mut out, &max, plain, 0);
    assert_eq!(max.status().submitted, 2);
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
    let layers = plain.layers_in(FHD_WIDTH, FHD_HEIGHT);
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
    serve_twice(&mut out, &max, mix, 2);
    assert_eq!(max.status().submitted, 4);
    let frame = read_shared_texture(&info).expect("a second WARP device reads Spout's texture");
    let fade = Composition::Fade {
        from: Some(nv12(&wide)),
        to: Some(nv12(&one)),
        weight_q8: weight_q8(4, 9),
    };
    assert_matches(&frame, &fade.layers(), size, "the Mix boundary");
    let frame = read_shared_texture(&fhd_info).expect("a second WARP device reads it");
    let layers = fade.layers_in(FHD_WIDTH, FHD_HEIGHT);
    assert_matches(&frame, &layers, fhd_size, "the Mix boundary in 1920x1080");

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
