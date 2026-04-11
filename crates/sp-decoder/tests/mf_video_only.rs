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
    assert_eq!(reader.width(), 32);
    assert_eq!(reader.height(), 32);
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
    assert_eq!(frame.width, 32);
    assert_eq!(frame.height, 32);
    assert!(!frame.data.is_empty());
    // 32×32 NV12: 32*32 Y plane + 32*16 UV plane = 1024 + 512 = 1536 bytes.
    assert_eq!(
        frame.data.len(),
        1536,
        "expected exactly 1536 bytes for 32×32 NV12 (Y + UV), got {}",
        frame.data.len()
    );
}
