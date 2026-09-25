//! SymphoniaAudioReader opens and decodes the committed FLAC fixtures.
//!
//! This test runs on every platform — Symphonia is pure Rust, so the audio
//! half of sp-decoder is no longer gated on Windows.
//!
//! ## Ramp fixtures (#148 v3 — seek trimming)
//!
//! `ramp_4096.flac` / `ramp_4608.flac` are 24-bit stereo 48 kHz, 3.000 s
//! (144 000 frames). Every frame `i` encodes its own index: left = `i`,
//! right = `-i` (as a 24-bit integer, so the decoded f32 is `i / 2^23`). The two
//! files differ ONLY in their FLAC block size (4096 vs 4608 frames), i.e. in
//! where symphonia's Accurate seek lands. After `seek(t)` the first decoded
//! sample must be frame `t * 48` exactly — never the start of the block that
//! contains it. See `fixtures/regen.sh`.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use sp_decoder::{AudioStream, MediaStream, StemMixReader, SymphoniaAudioReader, shared_gain};

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture() -> std::path::PathBuf {
    fixture_path("silent_3s.flac")
}

/// Frames per millisecond of the ramp fixtures (48 kHz).
const FRAMES_PER_MS: i64 = 48;
/// Total frames of each ramp fixture (3.000 s at 48 kHz).
const RAMP_FRAMES: i64 = 144_000;

/// Decode a ramp sample back to the frame index it encodes (`s * 2^23`).
fn ramp_index(sample: f32) -> i64 {
    (f64::from(sample) * 8_388_608.0).round() as i64
}

fn open_ramp(block: u32) -> SymphoniaAudioReader {
    SymphoniaAudioReader::open(&fixture_path(&format!("ramp_{block}.flac")))
        .expect("ramp fixture must open")
}

/// Read chunks until at least `min_frames` stereo frames are collected.
/// Returns the first chunk's `timestamp_ms` and the (left, right) frame indices.
fn collect_frames(reader: &mut dyn AudioStream, min_frames: usize) -> (u64, Vec<(i64, i64)>) {
    let mut first_ts = None;
    let mut frames = Vec::new();
    while frames.len() < min_frames {
        let chunk = reader
            .next_samples()
            .expect("decode should succeed")
            .expect("stream ended before enough frames were collected");
        assert_eq!(chunk.channels, 2);
        if first_ts.is_none() {
            first_ts = Some(chunk.timestamp_ms);
        }
        for f in chunk.data.chunks_exact(2) {
            frames.push((ramp_index(f[0]), ramp_index(f[1])));
        }
    }
    (first_ts.expect("at least one chunk"), frames)
}

/// Assert the collected frames continue the ramp from `first` with no gap:
/// frame `j` must encode EXACTLY `(first + j) * scale` (left) and its negation
/// (right). The decode is bit-exact (`n << 8` scaled by 2^-31 = `n / 2^23`), so
/// there is no tolerance: a one-frame-early seek (e.g. `SeekTo::Time` rounding
/// 288 ms down to frame 13 823) or a one-frame skew between stems fails.
fn assert_ramp_from(frames: &[(i64, i64)], first: i64, scale: i64, what: &str) {
    for (j, &(l, r)) in frames.iter().enumerate() {
        let want = (first + j as i64) * scale;
        assert!(
            l == want && r == -want,
            "{what}: frame {j} after seek encodes (L={l}, R={r}), expected (L={want}, R={})",
            -want
        );
    }
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

/// Guards the fixture itself: decoded from the start (no seek), every frame of
/// both ramp files encodes its own index, and the whole file is 144 000 frames.
#[test]
fn ramp_fixtures_encode_their_own_frame_index() {
    for block in [4096, 4608] {
        let mut reader = open_ramp(block);
        let mut frames = Vec::new();
        while let Some(chunk) = reader.next_samples().expect("decode should succeed") {
            for f in chunk.data.chunks_exact(2) {
                frames.push((ramp_index(f[0]), ramp_index(f[1])));
            }
        }
        assert_eq!(frames.len() as i64, RAMP_FRAMES, "ramp_{block} length");
        assert_ramp_from(&frames, 0, 1, &format!("ramp_{block} from start"));
    }
}

/// #148 v3: a seek INSIDE a FLAC block must trim the pre-target frames. With
/// 4096-frame blocks, `seek(1000)` targets frame 48 000 but symphonia lands on
/// the block starting at 45 056 — before the fix the first sample was 45 056
/// (61 ms early) while the chunk claimed `timestamp_ms = 1000`.
#[test]
fn seek_inside_a_block_starts_exactly_at_the_target_frame() {
    for block in [4096, 4608] {
        // 1000 → 48 000 (offset 2944 in a 4096 block, 1920 in a 4608 block),
        // 1500 → 72 000, 2345 → 112 560, 7 → 336 (inside the FIRST block).
        for t in [1_000_u64, 1_500, 2_345, 7] {
            let mut reader = open_ramp(block);
            reader.seek(t).expect("seek should succeed");
            // 10 000 frames spans several packets: the trim must not leak into
            // the packets that follow the first one.
            let (ts, frames) = collect_frames(&mut reader, 10_000);
            let target = t as i64 * FRAMES_PER_MS;
            assert_eq!(
                ts, t,
                "ramp_{block} seek({t}): first chunk timestamp_ms must be the target"
            );
            assert_ramp_from(&frames, target, 1, &format!("ramp_{block} seek({t})"));
        }
    }
}

/// A seek exactly ON a block boundary needs no trim and must stay exact:
/// 256 ms = frame 12 288 = 3 × 4096; 96 ms = frame 4608 = 1 × 4608;
/// 288 ms = frame 13 824 = 3 × 4608 — the case where a float `SeekTo::Time`
/// (0.288 s → 13 823.99… → 13 823) lands one frame early in the block before.
#[test]
fn seek_at_a_block_boundary_starts_exactly_at_the_target_frame() {
    for (block, t) in [(4096, 256_u64), (4608, 96), (4608, 288)] {
        let mut reader = open_ramp(block);
        reader.seek(t).expect("seek should succeed");
        let (ts, frames) = collect_frames(&mut reader, 6_000);
        assert_eq!(ts, t, "ramp_{block} seek({t}) timestamp_ms");
        assert_ramp_from(
            &frames,
            t as i64 * FRAMES_PER_MS,
            1,
            &format!("ramp_{block} boundary seek({t})"),
        );
    }
}

/// A second seek (after partially decoding the first) re-arms the trim from
/// scratch — no stale skip carried over, in either direction.
#[test]
fn consecutive_seeks_each_start_exactly_at_their_target() {
    let mut reader = open_ramp(4096);
    reader.seek(1_000).expect("seek 1");
    let _ = collect_frames(&mut reader, 100);
    for t in [2_000_u64, 500, 1_234] {
        reader.seek(t).expect("seek should succeed");
        let (ts, frames) = collect_frames(&mut reader, 5_000);
        assert_eq!(ts, t, "re-seek({t}) timestamp_ms");
        assert_ramp_from(
            &frames,
            t as i64 * FRAMES_PER_MS,
            1,
            &format!("re-seek({t})"),
        );
    }
}

/// Seek to 0 behaves as before: frame 0 first, labelled 0 ms.
#[test]
fn seek_to_zero_starts_at_frame_zero() {
    let mut reader = open_ramp(4608);
    let _ = collect_frames(&mut reader, 20_000);
    reader.seek(0).expect("seek(0) should succeed");
    let (ts, frames) = collect_frames(&mut reader, 5_000);
    assert_eq!(ts, 0);
    assert_ramp_from(&frames, 0, 1, "seek(0)");
}

/// Seek past the end behaves as before: an error, never a silent clamp.
#[test]
fn seek_past_the_end_is_an_error() {
    let mut reader = open_ramp(4096);
    assert!(
        reader.seek(10_000).is_err(),
        "seek beyond the 3 s ramp must be rejected"
    );
}

/// A rejected (out-of-range) seek does not move the reader, so a trim armed by
/// the previous successful seek still applies: playback resumes exactly at that
/// earlier target, labelled with it.
#[test]
fn rejected_seek_keeps_the_previous_seek_exact() {
    let mut reader = open_ramp(4096);
    reader.seek(1_000).expect("seek(1000) should succeed");
    assert!(reader.seek(10_000).is_err(), "out-of-range seek rejected");
    let (ts, frames) = collect_frames(&mut reader, 5_000);
    assert_eq!(ts, 1_000);
    assert_ramp_from(&frames, 1_000 * FRAMES_PER_MS, 1, "after rejected seek");
}

/// #148 v3: `StemMixReader` needs no code of its own — once every sub-reader
/// starts exactly at the target, the mix is aligned across stems. The two stems
/// use DIFFERENT block sizes, so before the fix they landed on different block
/// starts (45 056 vs 46 080 for `seek(1000)`) and the sum was misaligned.
/// With both gains at 1.0, output frame `j` must be `2 × (target + j)` (left)
/// and its negation (right) for every frame, across many packets of both.
#[test]
fn stem_mix_seek_is_aligned_across_stems_with_different_block_sizes() {
    let streams: Vec<Box<dyn AudioStream>> =
        vec![Box::new(open_ramp(4096)), Box::new(open_ramp(4608))];
    let gains: Vec<Arc<AtomicU32>> = vec![shared_gain(1.0), shared_gain(1.0)];
    let mut mix = StemMixReader::new(streams, gains).expect("stem mixer must build");
    for t in [1_000_u64, 2_345, 256, 288] {
        mix.seek(t).expect("seek should succeed");
        let (ts, frames) = collect_frames(&mut mix, 12_000);
        assert_eq!(ts, t, "stem-mix seek({t}) timestamp_ms");
        assert_ramp_from(
            &frames,
            t as i64 * FRAMES_PER_MS,
            2,
            &format!("stem-mix seek({t})"),
        );
    }
}
