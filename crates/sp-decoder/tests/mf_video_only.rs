//! MediaFoundationVideoReader opens a video-only MP4 fixture (no audio track).

#![cfg(windows)]

use sp_decoder::{MediaFoundationVideoReader, MediaStream, VideoStream};

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

#[test]
fn opens_video_only_mp4_and_reports_metadata() {
    let reader = MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    assert_eq!(reader.width(), 160);
    assert_eq!(reader.height(), 120);
    let (num, den) = reader.frame_rate();
    assert!(num > 0 && den > 0, "frame rate must be non-zero");
    let dur = reader.duration_ms();
    assert!(
        (2_500..=3_500).contains(&dur),
        "expected ~3000ms, got {dur}ms"
    );
}

#[test]
fn decodes_first_nv12_frame() {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    let frame = reader
        .next_frame()
        .expect("decode should succeed")
        .expect("first frame should exist");

    // Media Foundation hardware decoders pad frame dimensions to
    // alignment boundaries (typically 16 bytes). For a 160×120 source:
    //   width  160 → aligned 160 (already divisible by 16)
    //   height 120 → aligned 128 (next multiple of 16)
    // Assert the frame is at least as large as the source and no more
    // than one alignment block larger.
    assert!(
        frame.width >= 160 && frame.width <= 176,
        "width {} expected in [160, 176] (source 160 + ≤16 alignment)",
        frame.width
    );
    assert!(
        frame.height >= 120 && frame.height <= 136,
        "height {} expected in [120, 136] (source 120 + ≤16 alignment)",
        frame.height
    );
    assert!(!frame.data.is_empty());

    // NV12 layout is Y plane (stride × height) + UV plane (stride × height/2).
    // Data must be at least that large; real output is typically padded.
    let min_nv12 = (frame.stride as usize) * (frame.height as usize)
        + (frame.stride as usize) * (frame.height as usize / 2);
    assert!(
        frame.data.len() >= min_nv12,
        "expected at least {} bytes for {}×{} NV12 (stride {}), got {}",
        min_nv12,
        frame.width,
        frame.height,
        frame.stride,
        frame.data.len()
    );
    assert!(
        frame.stride >= frame.width,
        "stride {} must be >= width {}",
        frame.stride,
        frame.width
    );
}
