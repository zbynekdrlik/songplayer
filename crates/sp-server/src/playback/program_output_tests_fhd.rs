//! #223: `SP-program` is ALWAYS 1920×1080 (the owner's rule, ROZHODNUTÉ
//! 28.9.2026). Every picture the sender submits reaches the NDI backend as
//! the canvas — 1920×1080 NV12, stride 1920 — whatever its source's size: a
//! forwarded pair (the NDI input's capture is one too,
//! `ndi_input_tests_fhd.rs`), a fade's picture and the standby. A canvas
//! picture passes through as the same allocation (a decoder buffer with
//! slack past the picture goes out as it is); every picture the sender
//! MAKES — a fit, a fade's picture, the standby — is exactly 3 110 400 B. Any
//! other picture is fitted into the canvas, its aspect kept, with
//! studio-black bars (Y 16, UV 128) where the aspect differs.
//!
//! These tests run the PRODUCTION canvas (`ProgramOutput::fhd`, the
//! constructor `spawn_program_thread` uses), so every fit is a whole FHD
//! picture: keep their number small. The wire picture's bytes are read
//! through the submitter's async holdover (`FrameSubmitter::held_frame`), the
//! buffer the SDK still points at. #210's order (the VBAN hand-off before any
//! video work, so before the fit) is pinned in
//! `program_output_tests_order.rs`, whose forwarded pictures are fitted into
//! its 2×2 canvas. That the fit sits inside the `submit_us` span is
//! structural: `submit_video` reads `submit_start` before any picture work
//! (no test clock moves during a fit, so no test can see it).
//! Wired via `#[cfg(test)] #[path = "program_output_tests_fhd.rs"] mod tests_fhd;`.

use std::sync::Arc;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::ProgramOutput;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_transition::MixJob;
use crate::playback::submit_handoff::SubmitJob;

/// The canvas: 1920 × 1080 luma bytes, then 540 rows of 1920 chroma bytes.
const W: usize = 1920;
const H: usize = 1080;
const FHD_LEN: usize = 3_110_400;
/// The canvas's NDI send, as the mock records it.
const FHD_SEND: &str = "send_video_async(42,NV12,1920x1080,stride=1920,30/1)";
/// A padding byte of a stride wider than the picture: never shown.
const PAD: u8 = 7;
/// 2026-09 in 100 ns since the epoch.
const T0: i64 = 17_900_000_000_000_000;

fn fhd_output() -> (Arc<MockNdiBackend>, ProgramOutput<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    (backend, ProgramOutput::fhd(sender))
}

/// A `w`×`h` NV12 picture of row stride `stride` (the padding bytes [`PAD`]):
/// luma `y(column, row)`, every chroma pair `uv` (U, V).
fn picture(
    w: usize,
    h: usize,
    stride: usize,
    y: impl Fn(usize, usize) -> u8,
    uv: (u8, u8),
) -> Vec<u8> {
    let mut data = Vec::with_capacity(stride * (h + h / 2));
    for row in 0..h {
        data.extend((0..stride).map(|x| if x < w { y(x, row) } else { PAD }));
    }
    for _ in 0..h / 2 {
        data.extend((0..stride).map(|x| match (x < w, x % 2) {
            (false, _) => PAD,
            (true, 0) => uv.0,
            (true, _) => uv.1,
        }));
    }
    data
}

/// The luma of a `w`×`h` picture's four quadrants: 50 | 200 above its middle
/// row, 100 | 150 below. Scaled into the canvas it keeps them; cropped,
/// shifted or not scaled it does not.
fn quadrant(w: usize, h: usize, x: usize, row: usize) -> u8 {
    match (x < w / 2, row < h / 2) {
        (true, true) => 50,
        (false, true) => 200,
        (true, false) => 100,
        (false, false) => 150,
    }
}

/// One source pair on the first boundary: `video`, a `w`×`h` picture of row
/// stride `stride`, and one stereo block.
fn source(w: usize, h: usize, stride: usize, video: &SharedFrame) -> SubmitJob {
    let stamp = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    SubmitJob {
        width: w as u32,
        height: h as u32,
        stride: stride as u32,
        video: video.clone(),
        audio: vec![AudioFrame {
            data: vec![0.25; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp + 77,
        live: true,
    }
}

/// A source pair whose `w`×`h` picture is luma `y` and chroma `uv` throughout.
fn flat(w: usize, h: usize, y: u8, uv: (u8, u8)) -> SubmitJob {
    let video = SharedFrame::new(picture(w, h, w, |_, _| y, uv));
    source(w, h, w, &video)
}

/// The picture the sender put on the wire last, once its send is checked to
/// be the canvas's: the bytes the async holdover keeps alive for the SDK.
fn wire(backend: &MockNdiBackend, out: &ProgramOutput<MockNdiBackend>) -> SharedFrame {
    let sends: Vec<String> = backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_video_async("))
        .collect();
    assert_eq!(
        sends.last().map(String::as_str),
        Some(FHD_SEND),
        "the canvas: 1920×1080, stride 1920"
    );
    let held = out.submitter.held_frame().expect("a picture was sent");
    assert_eq!(
        backend.last_async_video_slice(),
        Some((held.as_ptr() as usize, held.len())),
        "the held picture is the one the SDK got"
    );
    held.clone()
}

fn luma_row(picture: &[u8], row: usize) -> &[u8] {
    &picture[row * W..(row + 1) * W]
}

fn chroma_row(picture: &[u8], row: usize) -> &[u8] {
    let at = W * H + row * W;
    &picture[at..at + W]
}

#[test]
fn every_source_size_reaches_sp_program_as_the_1920x1080_canvas() {
    // (source w, h; the canvas rows its picture fills: the first, how many).
    // 16:9 fills the canvas (scaled down, or up); 21:9 is letterboxed into
    // rows 134..944 (`nv12_fit::aspect_fit`: the 135-row bar rounds to 134).
    for (w, h, top, rows) in [
        (2560, 1440, 0, H),
        (1280, 720, 0, H),
        (2560, 1080, 134, 810),
    ] {
        let (backend, mut out) = fhd_output();
        let bytes = picture(w, h, w, |x, row| quadrant(w, h, x, row), (90, 170));
        let video = SharedFrame::new(bytes);
        out.submit(ProgramJob::Source(source(w, h, w, &video)));
        let wire = wire(&backend, &out);
        assert_eq!(wire.len(), FHD_LEN, "{w}×{h}: 3 110 400 B");
        assert!(
            !wire.ptr_eq(&video),
            "{w}×{h}: fitted into a buffer of its own"
        );
        for row in 0..H {
            let luma = luma_row(&wire, row);
            if !(top..top + rows).contains(&row) {
                assert!(
                    luma.iter().all(|&y| y == 16),
                    "{w}×{h}: luma row {row} is a studio-black bar"
                );
                continue;
            }
            // The middle rows and columns of an upscale blend two quadrants.
            let r = row - top;
            if r == rows / 2 - 1 || r == rows / 2 {
                continue;
            }
            for (x, &y) in luma.iter().enumerate() {
                if x == W / 2 - 1 || x == W / 2 {
                    continue;
                }
                assert_eq!(
                    y,
                    quadrant(W, rows, x, r),
                    "{w}×{h}: luma ({x}, {row}) shows its quadrant, scaled"
                );
            }
        }
        for row in 0..H / 2 {
            let chroma = chroma_row(&wire, row);
            if !(top / 2..(top + rows) / 2).contains(&row) {
                assert!(
                    chroma.iter().all(|&c| c == 128),
                    "{w}×{h}: chroma row {row} is a neutral bar"
                );
            } else {
                assert!(
                    chroma.chunks_exact(2).all(|uv| uv == [90u8, 170]),
                    "{w}×{h}: chroma row {row} keeps U 90 / V 170 interleaved"
                );
            }
        }
        assert_eq!(out.fit_plans(), 1, "{w}×{h}: one plan");
    }
}

#[test]
fn a_1920x1080_picture_passes_through_as_the_same_allocation() {
    let (backend, mut out) = fhd_output();
    let fhd = |stride| picture(W, H, stride, |x, row| quadrant(W, H, x, row), (90, 170));
    let exact = SharedFrame::new(fhd(W));
    assert_eq!(exact.len(), FHD_LEN);
    out.submit(ProgramJob::Source(source(W, H, W, &exact)));
    assert!(
        wire(&backend, &out).ptr_eq(&exact),
        "the source's own allocation, zero copy"
    );
    // A decoder buffer may carry slack past the picture: still a canvas
    // picture, sent as it is.
    let mut bytes = fhd(W);
    bytes.resize(FHD_LEN + 4096, 0);
    let slack = SharedFrame::new(bytes);
    out.submit(ProgramJob::Source(source(W, H, W, &slack)));
    assert!(
        wire(&backend, &out).ptr_eq(&slack),
        "slack past the picture: the source's own allocation"
    );
    assert_eq!(out.fit_plans(), 0, "nothing was fitted");
    // The standby black is the canvas too.
    out.submit(ProgramJob::Standby {
        stamp_100ns: floor_boundary_100ns(T0, GENLOCK_GRID_FPS),
    });
    let black = wire(&backend, &out);
    assert_eq!(black.len(), FHD_LEN);
    assert!(black[..W * H].iter().all(|&y| y == 16) && black[W * H..].iter().all(|&c| c == 128));
}

#[test]
fn a_1920x1080_picture_on_a_padded_stride_or_short_of_bytes_is_made_a_canvas_picture() {
    let (backend, mut out) = fhd_output();
    let fhd = |stride| picture(W, H, stride, |x, row| quadrant(W, H, x, row), (90, 170));
    // A decoder's padded stride: the picture is repacked onto stride 1920
    // (a fit at scale 1 copies every visible byte as it is).
    let padded = SharedFrame::new(fhd(2048));
    out.submit(ProgramJob::Source(source(W, H, 2048, &padded)));
    let wire_picture = wire(&backend, &out);
    assert_eq!(wire_picture.len(), FHD_LEN);
    assert!(
        wire_picture[..] == fhd(W)[..],
        "every visible byte as it was, no padding byte"
    );
    // A buffer short of the canvas's bytes is not a canvas picture: the fit
    // finds it is not whole NV12 and draws the canvas black, never a read
    // past its end.
    let mut bytes = fhd(W);
    bytes.truncate(FHD_LEN - 1);
    let short = SharedFrame::new(bytes);
    out.submit(ProgramJob::Source(source(W, H, W, &short)));
    let wire_picture = wire(&backend, &out);
    assert!(!wire_picture.ptr_eq(&short), "never the short buffer");
    assert_eq!(wire_picture.len(), FHD_LEN);
    assert!(
        wire_picture[..W * H].iter().all(|&y| y == 16)
            && wire_picture[W * H..].iter().all(|&c| c == 128),
        "the canvas black"
    );
    assert_eq!(out.fit_plans(), 2, "one plan per layout");
}

#[test]
fn a_fade_between_two_sizes_is_drawn_in_the_canvas() {
    let (backend, mut out) = fhd_output();
    let b0 = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    // 2560×1440 → 1280×720 on slot 4 of 9 (weight ½): both 16:9, both fill
    // the canvas, so every byte is the blend of the two flat pictures.
    out.submit(ProgramJob::Mix(MixJob {
        stamp_100ns: b0,
        from: Some(flat(2560, 1440, 200, (90, 170))),
        to: Some(flat(1280, 720, 40, (160, 100))),
        slot: 4,
        n_slots: 9,
    }));
    let mixed = wire(&backend, &out);
    assert_eq!(mixed.len(), FHD_LEN);
    assert!(
        mixed[..W * H].iter().all(|&y| y == 120),
        "(200 · 128 + 40 · 128 + 128) >> 8"
    );
    assert!(
        mixed[W * H..].chunks_exact(2).all(|uv| uv == [125u8, 135]),
        "U (90, 160) → 125, V (170, 100) → 135"
    );
    assert_eq!(out.fit_plans(), 2, "one plan per side");

    // 2560×1080 → a 1920×1080 canvas picture: the outgoing 21:9 picture's
    // bars blend with the incoming picture.
    out.submit(ProgramJob::Mix(MixJob {
        stamp_100ns: strict_next_boundary_100ns(b0, GENLOCK_GRID_FPS),
        from: Some(flat(2560, 1080, 200, (90, 170))),
        to: Some(flat(W, H, 40, (160, 100))),
        slot: 4,
        n_slots: 9,
    }));
    let mixed = wire(&backend, &out);
    assert_eq!(mixed.len(), FHD_LEN);
    for row in 0..H {
        let want = if (134..944).contains(&row) { 120 } else { 28 };
        assert!(
            luma_row(&mixed, row).iter().all(|&y| y == want),
            "luma row {row}: {want} (a bar: Y 16 blended with 40)"
        );
    }
    for row in 0..H / 2 {
        let want: [u8; 2] = if (67..472).contains(&row) {
            [125, 135]
        } else {
            [144, 114]
        };
        assert!(
            chroma_row(&mixed, row).chunks_exact(2).all(|uv| uv == want),
            "chroma row {row}: {want:?} (a bar: UV 128 blended with 160 / 100)"
        );
    }
    assert_eq!(
        out.fit_plans(),
        3,
        "the 21:9 plan; the canvas picture needs none"
    );
    assert_eq!(
        (out.mix_run.boundaries, out.mix_run.fitted),
        (2, 2),
        "two mixed boundaries, each with a side fitted"
    );
}

#[test]
fn a_fade_between_two_1440p_songs_scales_both_sides_in_one_pass() {
    // The box's regression case (#223 comment 5973492929): two 2560×1440
    // songs. Both sides are fitted into the canvas as they are read (#223
    // follow-up: one pass, no incoming canvas buffer first), by the ONE plan
    // their layout shares. The outgoing quadrants stay where they are,
    // scaled, under the incoming flat picture at weight ½.
    let (backend, mut out) = fhd_output();
    let (w, h) = (2560, 1440);
    let quadrants = SharedFrame::new(picture(w, h, w, |x, row| quadrant(w, h, x, row), (90, 170)));
    out.submit(ProgramJob::Mix(MixJob {
        stamp_100ns: floor_boundary_100ns(T0, GENLOCK_GRID_FPS),
        from: Some(source(w, h, w, &quadrants)),
        to: Some(flat(w, h, 40, (160, 100))),
        slot: 4,
        n_slots: 9,
    }));
    let mixed = wire(&backend, &out);
    assert_eq!(mixed.len(), FHD_LEN);
    for row in 0..H {
        if row == H / 2 - 1 || row == H / 2 {
            continue;
        }
        for (x, &y) in luma_row(&mixed, row).iter().enumerate() {
            if x == W / 2 - 1 || x == W / 2 {
                continue;
            }
            let want = match quadrant(W, H, x, row) {
                50 => 45,
                200 => 120,
                100 => 70,
                _ => 95,
            };
            assert_eq!(
                y, want,
                "luma ({x}, {row}): (q · 128 + 40 · 128 + 128) >> 8"
            );
        }
    }
    for row in 0..H / 2 {
        assert!(
            chroma_row(&mixed, row)
                .chunks_exact(2)
                .all(|uv| uv == [125u8, 135]),
            "chroma row {row}: U (90, 160) → 125, V (170, 100) → 135"
        );
    }
    assert_eq!(out.fit_plans(), 1, "one plan for both 1440p sides");
    assert_eq!(
        (out.mix_run.boundaries, out.mix_run.fitted),
        (1, 1),
        "one mixed boundary, fitted"
    );
}
