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
    assert_eq!(frame.width, 160);
    assert_eq!(frame.height, 120);
    assert!(!frame.data.is_empty());
    // 160×120 NV12: Y plane stride*height + UV plane stride*(height/2).
    // Hardware decoders often align stride (16/32/64-byte boundaries),
    // so we assert stride >= width and data length >= unpadded size.
    let min_nv12 = 160 * 120 + 160 * 60; // 28800 bytes unpadded
    assert!(
        frame.data.len() >= min_nv12,
        "expected at least {min_nv12} bytes for 160×120 NV12, got {}",
        frame.data.len()
    );
    assert!(
        frame.stride >= 160,
        "stride {} must be >= width 160",
        frame.stride
    );
}
