//! Regression test for the `duration_ms=0` bug (now against the split-file
//! video-only reader).

#![cfg(windows)]

use sp_decoder::{MediaFoundationVideoReader, MediaStream, VideoStream};

#[test]
fn mf_video_reader_reports_nonzero_duration_for_test_mp4() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4");
    assert!(fixture.exists());
    let reader = MediaFoundationVideoReader::open(&fixture).expect("open");
    let duration = reader.duration_ms();
    assert!(
        (2_500..=3_500).contains(&duration),
        "expected ~3000ms, got {duration}ms"
    );
}

#[test]
fn mf_video_reader_reports_nonzero_size() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4");
    let reader = MediaFoundationVideoReader::open(&fixture).expect("open");
    assert_eq!(reader.width(), 160);
    assert_eq!(reader.height(), 120);
    let (num, den) = reader.frame_rate();
    assert!(num > 0 && den > 0);
}
