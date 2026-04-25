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

/// Seek-to-zero must not throw, and the reader must still return a frame.
/// Covers the regression introduced by commit 5977a9d where using
/// `windows::core::PROPVARIANT::from(i64)` in the seek impl broke later
/// `ReadSample` calls under release-mode LTO.
#[test]
fn seek_zero_then_next_frame_succeeds() {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    reader
        .seek(0)
        .expect("seek(0) must succeed on a seekable file");
    let frame = reader
        .next_frame()
        .expect("next_frame must succeed after seek")
        .expect("first frame should exist after seek(0)");
    assert!(!frame.data.is_empty(), "frame must have pixel data");
}

/// Mid-point seek does not error and still yields a frame.
/// We do NOT assert on the returned timestamp: MF snaps to the nearest
/// prior keyframe, and the 3-second `black_3s.mp4` fixture was generated
/// with only a single keyframe at ts=0 (see `tests/fixtures/regen.sh`).
/// A production movie has keyframes every 1-2 s so the returned frame
/// would land near the target, but the test must pass on the fixture.
#[test]
fn seek_midpoint_does_not_error_and_returns_frame() {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    let dur = reader.duration_ms();
    assert!(dur >= 1_000, "fixture must be long enough to seek");
    let target_ms = dur / 2;
    reader.seek(target_ms).expect("seek must succeed");
    let frame = reader
        .next_frame()
        .expect("next_frame after seek must succeed")
        .expect("a frame must exist after seek");
    assert!(!frame.data.is_empty(), "frame must have pixel data");
}

/// Regression gate: after a seek on one reader, an INDEPENDENT new reader
/// must still decode frames. This was the exact failure mode observed on
/// 2026-04-22 (commit 5977a9d) — any seek call left MF in a state where
/// later `MediaFoundationVideoReader::open` succeeded but `next_frame`
/// returned EOS immediately (frame_count=0 for every new song).
#[test]
fn seek_on_one_reader_does_not_break_next_reader() {
    {
        let mut r1 = MediaFoundationVideoReader::open(&fixture()).expect("open r1");
        // Deliberately exercise seek so any global-state corruption triggers.
        r1.seek(500).expect("seek r1");
    } // r1 dropped

    let mut r2 = MediaFoundationVideoReader::open(&fixture()).expect("open r2");
    let frame = r2
        .next_frame()
        .expect("next_frame on fresh reader must not error")
        .expect("fresh reader must yield a frame, not EOS");
    assert!(!frame.data.is_empty());
}
