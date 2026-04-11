//! Regression test for the `duration_ms=0` bug.
//!
//! Before the fix, `MediaReader::open` set `duration_ms: 0` and only
//! updated it as frames were decoded. This meant `PipelineEvent::Started`
//! always fired with `duration_ms: 0`, which in turn broke the
//! title-hide 3.5s-before-end timer and the dashboard progress bar.
//!
//! The fix reads `MF_PD_DURATION` from the source reader's
//! `MF_SOURCE_READER_MEDIASOURCE` sentinel at `open` time. This test
//! exercises a tiny committed MP4 fixture to verify the duration is
//! reported accurately before any frames are decoded.

#![cfg(windows)]

use sp_decoder::MediaReader;

#[test]
fn media_reader_reports_nonzero_duration_for_test_mp4() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("silent_3s.mp4");

    assert!(
        fixture.exists(),
        "fixture file should exist at {}",
        fixture.display()
    );

    let reader = MediaReader::open(&fixture).expect("MediaReader::open should succeed");
    let duration = reader.duration_ms();

    // Fixture is a 3.000s black video + silent stereo audio. MF typically
    // reports the longer of audio/video; either way we allow a ±500ms
    // window to cover container rounding.
    assert!(
        duration >= 2_500 && duration <= 3_500,
        "expected ~3000ms duration before any frames are decoded, got {duration}ms"
    );
}

#[test]
fn media_reader_reports_nonzero_video_size() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("silent_3s.mp4");

    let reader = MediaReader::open(&fixture).expect("MediaReader::open should succeed");
    let info = reader.video_info();
    assert_eq!(info.width, 160);
    assert_eq!(info.height, 120);
    assert!(info.frame_rate_num > 0);
    assert!(info.frame_rate_den > 0);
}
