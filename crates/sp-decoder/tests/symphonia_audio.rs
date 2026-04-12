//! SymphoniaAudioReader opens and decodes the committed FLAC fixture.
//!
//! This test runs on every platform — Symphonia is pure Rust, so the audio
//! half of sp-decoder is no longer gated on Windows.

use sp_decoder::{AudioStream, MediaStream, SymphoniaAudioReader};

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("silent_3s.flac")
}

#[test]
fn opens_flac_and_reports_metadata() {
    let reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    assert_eq!(reader.sample_rate(), 48_000);
    assert_eq!(reader.channels(), 2);
    let dur = reader.duration_ms();
    assert!(
        (2_900..=3_100).contains(&dur),
        "expected ~3000ms, got {dur}ms"
    );
}

#[test]
fn decodes_first_chunk_with_valid_samples() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    let chunk = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("chunk should exist");
    assert_eq!(chunk.channels, 2);
    assert_eq!(chunk.sample_rate, 48_000);
    assert!(!chunk.data.is_empty(), "first chunk must contain samples");
    // Silence: every sample is ~0.0. Allow tiny FLAC quantisation noise.
    let max_abs = chunk.data.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
    assert!(max_abs < 1e-4, "silence expected, max |s| = {max_abs}");
}

#[test]
fn decodes_entire_fixture_to_expected_sample_count() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    let mut total_samples = 0_usize;
    while let Some(chunk) = reader.next_samples().expect("decode should succeed") {
        // Interleaved samples: count frames (1 frame = channels samples).
        assert_eq!(chunk.channels, 2);
        total_samples += chunk.data.len() / 2;
    }
    // 3.000 seconds * 48_000 Hz = 144_000 frames, ±1 frame tolerance for
    // block boundary rounding inside the FLAC encoder.
    assert!(
        (143_999..=144_001).contains(&total_samples),
        "expected ~144000 frames, got {total_samples}"
    );
}

#[test]
fn seek_to_midpoint_reports_matching_timestamp() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    reader.seek(1_500).expect("seek should succeed");
    let chunk = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("post-seek chunk should exist");
    let ts = chunk.timestamp_ms;
    assert!(
        (1_450..=1_550).contains(&ts),
        "expected ~1500ms after seek, got {ts}ms"
    );
}
