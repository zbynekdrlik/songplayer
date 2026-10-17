//! #282: the reader's decode cap on the H.264 fixture (160×120 testsrc2,
//! 30 fps), on the software path.
//!
//! `windows-latest` has no GPU and its WARP refuses the video device, so no
//! reader here reaches the D3D11 path (`mf_hw_decode.rs`): the GPU video
//! processor's scale is proven on the box only. What runs here is the same
//! open — a first source reader read for the stream's own size, then a
//! second one with `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` and
//! the NV12 output type's `MF_MT_FRAME_SIZE` — with Media Foundation's video
//! processor scaling on the CPU.
//!
//! Every test that changes or reads the process-wide `hw_counters()` holds
//! `COUNTERS`, so a counter's change is that test's own and is pinned
//! exactly.

#![cfg(windows)]

use std::sync::{Mutex, MutexGuard};

use sp_decoder::{
    DecodeMode, DecodedVideoFrame, MediaFoundationVideoReader, VideoStream, hw_counters,
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

/// The mean of the top-left `w`×`h` luma.
fn mean_luma(frame: &DecodedVideoFrame, w: usize, h: usize) -> f64 {
    let stride = frame.stride as usize;
    let mut sum = 0_u64;
    for y in 0..h {
        for &v in &frame.data[y * stride..y * stride + w] {
            sum += u64::from(v);
        }
    }
    sum as f64 / (w * h) as f64
}

fn software_pictures() -> Vec<DecodedVideoFrame> {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    decode_all(&mut reader, "unscaled")
}

/// A cap of 60 rows: the box is 106×60 (60 × 16/9 = 106.7 → 106), the
/// 160×120 picture's height binds, so it is decoded at 80×60.
#[test]
fn a_cap_smaller_than_the_picture_hands_every_picture_over_scaled() {
    let _alone = counters_alone();
    let before = hw_counters().snapshot();
    let mut reader =
        MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(60))
            .expect("scaled open");
    let after = hw_counters().snapshot();
    assert_eq!(
        after.scaled_opens,
        before.scaled_opens + 1,
        "the open was scaled"
    );
    assert_eq!(
        after.scale_refusals, before.scale_refusals,
        "Media Foundation took the scaled type: {:?}",
        after
    );
    assert_eq!(
        (reader.width(), reader.height()),
        (80, 60),
        "the reader reports the scaled size"
    );
    let scaled = decode_all(&mut reader, "scaled");
    let plain = software_pictures();
    assert_eq!(
        scaled.len(),
        plain.len(),
        "one scaled picture per decoded picture (no frame-rate conversion)"
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
        // The same picture, smaller: its mean luma stays (a crop, a range
        // conversion or garbage would move it).
        let (scaled_mean, plain_mean) = (mean_luma(s, 80, 60), mean_luma(p, W, H));
        assert!(
            (scaled_mean - plain_mean).abs() < 8.0,
            "picture {i}: mean luma {scaled_mean:.1} scaled vs {plain_mean:.1} unscaled"
        );
    }
}

/// A cap whose box holds the picture opens it exactly as before: no
/// scaled open, the same pictures as an uncapped reader.
#[test]
fn a_cap_at_or_above_the_picture_changes_nothing() {
    let _alone = counters_alone();
    let plain = software_pictures();
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
