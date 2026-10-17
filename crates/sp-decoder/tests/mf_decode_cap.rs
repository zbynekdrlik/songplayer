//! #282: the reader's decode cap on the H.264 fixture (160×120 testsrc2,
//! 30 fps), on the software path.
//!
//! `windows-latest` has no GPU and its WARP refuses the video device, so no
//! reader here reaches the D3D11 path (`mf_hw_decode.rs`): the GPU video
//! processor's scale is proven on the box only. What runs here is the same
//! open — a first source reader read for the stream's own size, then a
//! second one with `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` and
//! the NV12 output type's `MF_MT_FRAME_SIZE` (+ the native frame rate and
//! the pixel aspect that keeps the display aspect) — with Media
//! Foundation's video processor scaling on the CPU.
//!
//! The picture checks compare the scaled picture with the unscaled one by
//! regions, with tolerances measured on this fixture (ffmpeg's bilinear,
//! bicubic, area, lanczos and nearest scalers standing in for the video
//! processor, against a crop, a 1–2 px letterbox or pillarbox, a
//! limited→full range conversion, a BT.601→709 matrix change, 8 padding
//! rows scaled in and a 2-row shift; review round 1):
//!
//! - luma means of a 4×4 grid: a correct scale ≤ 2.7, a defect ≥ 10.9
//!   (tolerance 6);
//! - the edge bands (2 rows / columns of the scaled picture against the
//!   matching rows / columns of the unscaled one): a correct scale ≤ 0.5 at
//!   80×60 and ≤ 4.8 at 66×50, a defect ≥ 9.9 (tolerance 6 and 10).
//!
//! Every test that changes or reads the process-wide `hw_counters()` (or
//! arms the refusal hook) holds `COUNTERS`, so a counter's change is that
//! test's own and is pinned exactly.

#![cfg(windows)]

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use sp_decoder::{
    DecodeMode, DecodedVideoFrame, MediaFoundationVideoReader, MediaStream, VideoStream,
    hw_counters,
};

/// The fixture's own picture.
const W: usize = 160;
const H: usize = 120;

/// One counter-touching test at a time (the module doc).
static COUNTERS: Mutex<()> = Mutex::new(());

fn counters_alone() -> MutexGuard<'static, ()> {
    COUNTERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

/// Every picture of `reader`.
fn decode_all(reader: &mut MediaFoundationVideoReader, what: &str) -> Vec<DecodedVideoFrame> {
    let mut pictures = Vec::new();
    while let Some(frame) = reader
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: decode {}: {e}", pictures.len()))
    {
        pictures.push(frame);
    }
    assert!(pictures.len() > 30, "{what}: {} pictures", pictures.len());
    pictures
}

/// The top-left `w`×`h` of a picture, Y rows then UV rows, read through the
/// picture's own stride and height (the layout every consumer reads).
fn visible(frame: &DecodedVideoFrame, w: usize, h: usize) -> Vec<u8> {
    let stride = frame.stride as usize;
    let rows = frame.height as usize;
    assert!(
        stride >= w && rows >= h,
        "{}x{} stride {stride}",
        frame.width,
        rows
    );
    assert!(
        frame.data.len() >= stride * (rows + rows.div_ceil(2)),
        "a whole NV12 picture: {} bytes, stride {stride}, {rows} rows",
        frame.data.len()
    );
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        out.extend_from_slice(&frame.data[y * stride..y * stride + w]);
    }
    let uv = stride * rows;
    for y in 0..h.div_ceil(2) {
        let row = uv + y * stride;
        out.extend_from_slice(&frame.data[row..row + w]);
    }
    out
}

/// The mean luma of the rows `y0..y1` and columns `x0..x1`.
fn luma_mean(frame: &DecodedVideoFrame, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> f64 {
    let stride = frame.stride as usize;
    let mut sum = 0_u64;
    for y in y0..y1 {
        for &v in &frame.data[y * stride + x0..y * stride + x1] {
            sum += u64::from(v);
        }
    }
    sum as f64 / ((x1 - x0) * (y1 - y0)) as f64
}

/// The luma means of a 4×4 grid over the `w`×`h` picture (both divisible
/// by 4).
fn grid(frame: &DecodedVideoFrame, w: usize, h: usize) -> Vec<f64> {
    let (bw, bh) = (w / 4, h / 4);
    let mut out = Vec::new();
    for by in 0..4 {
        for bx in 0..4 {
            out.push(luma_mean(
                frame,
                (bx * bw, (bx + 1) * bw),
                (by * bh, (by + 1) * bh),
            ));
        }
    }
    out
}

/// The luma means of the top, bottom, left and right `edge` rows / columns
/// of the `w`×`h` picture.
fn bands(frame: &DecodedVideoFrame, w: usize, h: usize, edge: usize) -> Vec<f64> {
    vec![
        luma_mean(frame, (0, w), (0, edge)),
        luma_mean(frame, (0, w), (h - edge, h)),
        luma_mean(frame, (0, edge), (0, h)),
        luma_mean(frame, (w - edge, w), (0, h)),
    ]
}

/// The largest difference between two lists of means.
fn worst(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}

/// An uncapped reader on the fixture, its facts, and every picture.
fn unscaled() -> (MediaFoundationVideoReader, Vec<DecodedVideoFrame>) {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    let pictures = decode_all(&mut reader, "unscaled");
    (reader, pictures)
}

/// A capped reader's open, its facts against the unscaled reader's, and
/// the counters' change; it writes what Media Foundation negotiated
/// straight to stderr (past libtest's capture), so every CI log shows it.
fn open_scaled(
    cap: u32,
    size: (u32, u32),
    plain: &MediaFoundationVideoReader,
) -> MediaFoundationVideoReader {
    let before = hw_counters().snapshot();
    let reader = MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(cap))
        .expect("scaled open");
    let after = hw_counters().snapshot();
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap {cap}: {}x{} at {:?} (known {}), duration {} ms, scaled opens +{}, refusals +{}",
        reader.width(),
        reader.height(),
        reader.frame_rate(),
        reader.frame_rate_known(),
        reader.duration_ms(),
        after.scaled_opens - before.scaled_opens,
        after.scale_refusals - before.scale_refusals
    );
    assert_eq!(
        after.scale_refusals, before.scale_refusals,
        "cap {cap}: Media Foundation took the scaled type"
    );
    assert_eq!(
        after.scaled_opens,
        before.scaled_opens + 1,
        "cap {cap}: the open was scaled"
    );
    assert_eq!((reader.width(), reader.height()), size, "cap {cap}");
    assert_eq!(
        reader.frame_rate(),
        plain.frame_rate(),
        "cap {cap}: the stream's own rate, no conversion"
    );
    assert!(reader.frame_rate_known(), "cap {cap}: not the 29.97 guess");
    reader
}

/// A cap of 60 rows: the box is 106×60 (60 × 16/9 = 106.7 → 106), the
/// 160×120 picture's height binds, so it is decoded at 80×60 (pixel aspect
/// 1:1).
#[test]
fn a_cap_smaller_than_the_picture_hands_every_picture_over_scaled() {
    let _alone = counters_alone();
    let (plain_reader, plain) = unscaled();
    let mut reader = open_scaled(60, (80, 60), &plain_reader);
    let scaled = decode_all(&mut reader, "scaled");
    assert_eq!(
        scaled.len(),
        plain.len(),
        "one scaled picture per decoded picture (no frame-rate conversion)"
    );
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap 60: first picture {}x{} stride {}",
        scaled[0].width,
        scaled[0].height,
        scaled[0].stride
    );
    for (i, (s, p)) in scaled.iter().zip(&plain).enumerate() {
        assert_eq!(s.timestamp_ms, p.timestamp_ms, "picture {i}: its time");
        assert_eq!(
            (s.width, s.height, s.stride),
            (80, 60, 80),
            "picture {i}: the scaled size, packed (stride = one chroma row)"
        );
        // A whole NV12 picture, every byte of both planes readable.
        let _ = visible(s, 80, 60);
        let grid_diff = worst(&grid(s, 80, 60), &grid(p, W, H));
        assert!(
            grid_diff < 6.0,
            "picture {i}: a 4x4 luma block off by {grid_diff:.1}"
        );
        let edge_diff = worst(&bands(s, 80, 60, 2), &bands(p, W, H, 4));
        assert!(
            edge_diff < 6.0,
            "picture {i}: an edge band off by {edge_diff:.1} (a letterbox, a crop or padding)"
        );
    }
}

/// A cap of 50 rows: the box is 88×50, the picture is decoded at 66×50, a
/// fraction of a pixel off the native aspect (pixel aspect 100:99): the
/// picture must still fill the frame (no letterboxed edge).
#[test]
fn an_inexact_ratio_fills_the_whole_frame() {
    let _alone = counters_alone();
    let (plain_reader, plain) = unscaled();
    let mut reader = open_scaled(50, (66, 50), &plain_reader);
    let scaled = decode_all(&mut reader, "scaled at 66x50");
    assert_eq!(scaled.len(), plain.len());
    for (i, (s, p)) in scaled.iter().zip(&plain).enumerate() {
        assert_eq!(s.timestamp_ms, p.timestamp_ms, "picture {i}: its time");
        assert_eq!((s.width, s.height), (66, 50), "picture {i}");
        let _ = visible(s, 66, 50);
        // 2 scaled rows / columns ≈ 5 unscaled ones (×2.4).
        let edge_diff = worst(&bands(s, 66, 50, 2), &bands(p, W, H, 5));
        assert!(
            edge_diff < 10.0,
            "picture {i}: an edge band off by {edge_diff:.1} (a letterboxed edge)"
        );
    }
}

/// A cap whose box holds the picture opens it exactly as before: no
/// scaled open, the same pictures as an uncapped reader.
#[test]
fn a_cap_at_or_above_the_picture_changes_nothing() {
    let _alone = counters_alone();
    let (_, plain) = unscaled();
    let before = hw_counters().snapshot();
    for cap in [120, 2160] {
        let mut reader =
            MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(cap))
                .expect("open");
        assert_eq!((reader.width(), reader.height()), (160, 120), "cap {cap}");
        let pictures = decode_all(&mut reader, "capped, inside the box");
        assert_eq!(pictures.len(), plain.len(), "cap {cap}");
        for (i, (c, p)) in pictures.iter().zip(&plain).enumerate() {
            assert_eq!(
                (c.timestamp_ms, c.width, c.height, c.stride),
                (p.timestamp_ms, p.width, p.height, p.stride),
                "cap {cap}, picture {i}"
            );
            assert!(
                visible(c, W, H) == visible(p, W, H),
                "cap {cap}, picture {i}: the same bytes"
            );
        }
    }
    let after = hw_counters().snapshot();
    assert_eq!(after.scaled_opens, before.scaled_opens, "nothing scaled");
    assert_eq!(after.scale_refusals, before.scale_refusals);
}

/// Review round 1: a scaled open Media Foundation refuses plays the file at
/// its own size (never a dead song), counted in `scale_refusals`. CI's
/// Media Foundation takes the scaled type, so the refusal is injected.
#[test]
fn a_refused_scaled_open_plays_the_file_at_its_own_size() {
    let _alone = counters_alone();
    let (_, plain) = unscaled();
    let before = hw_counters().snapshot();
    MediaFoundationVideoReader::refuse_next_scaled_open_for_test();
    let mut reader =
        MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(60))
            .expect("a refused scaled open still opens the file");
    let after = hw_counters().snapshot();
    assert_eq!(after.scale_refusals, before.scale_refusals + 1, "counted");
    assert_eq!(after.scaled_opens, before.scaled_opens, "not scaled");
    assert_eq!((reader.width(), reader.height()), (160, 120));
    let pictures = decode_all(&mut reader, "refused, unscaled");
    assert_eq!(pictures.len(), plain.len());
    for (i, (c, p)) in pictures.iter().zip(&plain).enumerate() {
        assert_eq!(
            (c.timestamp_ms, c.width, c.height),
            (p.timestamp_ms, p.width, p.height),
            "picture {i}"
        );
        assert!(
            visible(c, W, H) == visible(p, W, H),
            "picture {i}: the same bytes as an uncapped reader"
        );
    }
}
