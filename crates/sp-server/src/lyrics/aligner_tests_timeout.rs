//! RED unit + structural guards (#144, "Rollout blocker #3") for the
//! duration-scaled vocal-isolation timeout and the soundfile stem writer.
//!
//! Vocal isolation (Mel-Roformer + anvuew dereverb + resample) runs at ≈1×
//! realtime on win-resolume (measured 2026-09-12: 240-s song → 233 s; the
//! Mel-Roformer pass alone is 0.75× realtime and linear on a 240-s and an
//! 827-s song). The old hard-coded 600 s ceiling could only ever pass a
//! song under ~9 min from a cached WAV — the 10–15-min band (29 catalog
//! songs) timed out on every cold run. `isolation_timeout` scales the
//! ceiling to the song's length.

use super::*;
use std::time::Duration;

#[test]
fn isolation_timeout_clamps_short_song_up_to_floor() {
    // 240 s song → 2× = 480 s, clamped up to the 600 s floor.
    assert_eq!(isolation_timeout(Some(240_000)), Duration::from_secs(600));
}

#[test]
fn isolation_timeout_scales_linearly_in_band() {
    // 827 s song (the measured long song) → 2× = 1654 s, within the band.
    assert_eq!(isolation_timeout(Some(827_000)), Duration::from_secs(1654));
}

#[test]
fn isolation_timeout_clamps_long_song_to_ceiling() {
    // ~70-min live set → 2× far exceeds the ceiling, clamped to 3600 s.
    assert_eq!(
        isolation_timeout(Some(4_243_000)),
        Duration::from_secs(3600)
    );
}

#[test]
fn isolation_timeout_unknown_duration_uses_ceiling() {
    assert_eq!(isolation_timeout(None), Duration::from_secs(3600));
}

#[test]
fn isolation_timeout_nonpositive_duration_uses_ceiling() {
    assert_eq!(isolation_timeout(Some(0)), Duration::from_secs(3600));
}

/// Structural: the magic 600 s literal must be gone from `preprocess_vocals`
/// and the ceiling must arrive as a parameter. Scoped to the function body
/// (the `isolation_timeout` clamp legitimately keeps a `from_secs(600)`
/// floor elsewhere in the file). CRLF-normalised for the Windows CI checkout.
#[test]
fn preprocess_vocals_takes_timeout_parameter_no_magic_600() {
    let src = include_str!("aligner.rs").replace("\r\n", "\n");
    let start = src
        .find("pub async fn preprocess_vocals(")
        .expect("preprocess_vocals must exist");
    let after = &src[start..];
    let end = after
        .find("pub async fn align_chunks")
        .unwrap_or(after.len());
    let body = &after[..end];
    assert!(
        body.contains("timeout: std::time::Duration"),
        "preprocess_vocals must take `timeout: std::time::Duration` as a parameter"
    );
    assert!(
        !body.contains("from_secs(600)"),
        "preprocess_vocals must no longer hard-code a 600 s timeout"
    );
}

/// Structural: `cmd_preprocess_vocals` must write both isolation stems via
/// the soundfile writer, not pydub — pydub's writer runs out of memory on
/// long 24-bit stems (pydub#135; observed on an 827-s song 2026-09-12).
/// Exactly two occurrences: the two `Separator(...)` calls in that function
/// (the script's other Separators keep the default writer). CRLF-normalised.
#[test]
fn preprocess_vocals_script_uses_soundfile_writer_twice() {
    let src = include_str!("../../../../scripts/lyrics_worker.py").replace("\r\n", "\n");
    let count = src.matches("use_soundfile=True").count();
    assert_eq!(
        count, 2,
        "cmd_preprocess_vocals must set the soundfile writer on both Separator() calls (found {count})"
    );
}
