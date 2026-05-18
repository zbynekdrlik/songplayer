//! Tests for `lyrics::whisperx_replicate`. Sibling file referenced by
//! `whisperx_replicate.rs` under
//! `#[path = "whisperx_replicate_tests.rs"] #[cfg(test)] mod tests;`
//! to keep the parent under the 1000-line airuleset cap.

use super::*;
use crate::lyrics::audio_chunking::ChunkPlan;

// ── merge_chunk_lines tests (issue #65 — chunk-merge invariant) ────────────

fn word(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.into(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn line(text: &str, start_ms: u32, end_ms: u32, words: Option<Vec<AlignedWord>>) -> AlignedLine {
    AlignedLine {
        text: text.into(),
        start_ms,
        end_ms,
        words,
    }
}

#[test]
fn merge_first_chunk_passes_all_lines_no_offset() {
    // Chunk 0 [0..60000ms]: drop_below_ms = 0; offset = 0; no lines filtered.
    let plan = ChunkPlan {
        idx: 0,
        start_ms: 0,
        end_ms: 60_000,
    };
    let input = vec![
        line("Amazing grace", 100, 1500, None),
        line("How sweet the sound", 1500, 4000, None),
    ];
    let out = merge_chunk_lines(input.clone(), &plan);
    assert_eq!(out, input, "first chunk passes everything through");
}

#[test]
fn merge_offsets_subsequent_chunks_into_global_timing() {
    // Chunk 1 [50_000..110_000ms]: offset = 50_000.
    let plan = ChunkPlan {
        idx: 1,
        start_ms: 50_000,
        end_ms: 110_000,
    };
    // chunk-local start at 12_000 → global 62_000 (above 60_000 drop boundary)
    let input = vec![line("after overlap", 12_000, 14_000, None)];
    let out = merge_chunk_lines(input, &plan);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].start_ms, 62_000);
    assert_eq!(out[0].end_ms, 64_000);
}

#[test]
fn merge_drops_overlap_region_for_subsequent_chunks() {
    // Chunk 1 [50_000..110_000ms]: drop_below_ms = 50_000 + 10_000 = 60_000.
    // chunk-local 0..10_000 maps to global 50_000..60_000 — entirely
    // inside the overlap region of chunk 0; must be dropped.
    let plan = ChunkPlan {
        idx: 1,
        start_ms: 50_000,
        end_ms: 110_000,
    };
    let input = vec![
        line("overlap with prev", 2_000, 8_000, None), // global 52..58k → drop
        line("on boundary", 10_000, 11_000, None),     // global 60..61k → keep
        line("clearly past overlap", 30_000, 31_000, None), // global 80..81k → keep
    ];
    let out = merge_chunk_lines(input, &plan);
    assert_eq!(out.len(), 2, "first line dropped, two kept");
    assert_eq!(out[0].start_ms, 60_000);
    assert_eq!(out[1].start_ms, 80_000);
}

#[test]
fn merge_offsets_word_timings_too() {
    let plan = ChunkPlan {
        idx: 1,
        start_ms: 50_000,
        end_ms: 110_000,
    };
    let input = vec![line(
        "two words",
        12_000,
        14_000,
        Some(vec![
            word("two", 12_000, 13_000),
            word("words", 13_000, 14_000),
        ]),
    )];
    let out = merge_chunk_lines(input, &plan);
    let words = out[0].words.as_ref().expect("words preserved");
    assert_eq!(words[0].start_ms, 62_000);
    assert_eq!(words[0].end_ms, 63_000);
    assert_eq!(words[1].start_ms, 63_000);
    assert_eq!(words[1].end_ms, 64_000);
}

#[test]
fn merge_drops_below_boundary_strictly_less_than() {
    // The dedup rule is: drop when `global_start < drop_below_ms`.
    // A line whose global_start == drop_below_ms must be KEPT.
    let plan = ChunkPlan {
        idx: 1,
        start_ms: 50_000,
        end_ms: 110_000,
    };
    // chunk-local 10_000 → global 60_000 == drop_below_ms → keep
    let input = vec![line("exact boundary", 10_000, 11_000, None)];
    let out = merge_chunk_lines(input, &plan);
    assert_eq!(out.len(), 1);
}

#[test]
fn merge_empty_chunk_yields_empty() {
    let plan = ChunkPlan {
        idx: 0,
        start_ms: 0,
        end_ms: 60_000,
    };
    let out = merge_chunk_lines(Vec::new(), &plan);
    assert!(out.is_empty());
}

// ── probe_duration_ms tests (kill mutation survivors) ─────────────────────

/// Build a minimal WAV byte stream: RIFF header + WAVE + fmt chunk
/// (16-bit PCM, configurable byte_rate) + data chunk (configurable size).
fn build_wav(byte_rate: u32, data_size: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36u32 + data_size).to_le_bytes()); // file size
    buf.extend_from_slice(b"WAVE");
    // fmt chunk: size 16, audio_format=1 (PCM), channels=1, sample_rate,
    // byte_rate, block_align=2, bits_per_sample=16.
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // 1 channel
    buf.extend_from_slice(&16000u32.to_le_bytes()); // sample_rate
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block_align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits_per_sample
    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend(std::iter::repeat_n(0u8, data_size as usize));
    buf
}

fn write_temp_wav(byte_rate: u32, data_size: u32) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&build_wav(byte_rate, data_size)).unwrap();
    tmp.flush().unwrap();
    tmp
}

#[test]
fn probe_duration_ms_one_second_wav() {
    // byte_rate = 32000 (16kHz × 2 bytes/sample), data_size = 32000 → 1 s.
    // Mutation `Ok(0)` and `Ok(1)` short-circuit return the wrong constant;
    // the arithmetic mutations on `data_size * 1000 / byte_rate` change
    // the result. Fixed-input arithmetic test catches all of them.
    let wav = write_temp_wav(32000, 32000);
    let ms = probe_duration_ms(wav.path()).unwrap();
    assert_eq!(ms, 1000, "32000 bytes / 32000 byte_rate × 1000 = 1000 ms");
}

#[test]
fn probe_duration_ms_half_second_wav() {
    // 16000 bytes at 32000 byte_rate = 500 ms. Independent value to
    // discriminate `* with +` vs original arithmetic.
    let wav = write_temp_wav(32000, 16000);
    let ms = probe_duration_ms(wav.path()).unwrap();
    assert_eq!(ms, 500);
}

#[test]
fn probe_duration_ms_rejects_non_wav_header() {
    // Header `RIFX...JUNK` — fails the `RIFF` || `WAVE` magic check.
    // Mutation `||` ↔ `&&` would require BOTH to be wrong simultaneously
    // before rejecting. Mutation `!=` ↔ `==` would invert the check.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"RIFX\x00\x00\x00\x00JUNKaaaa").unwrap();
    tmp.flush().unwrap();
    let result = probe_duration_ms(tmp.path());
    assert!(matches!(result, Err(BackendError::Malformed(_))));
}

#[test]
fn probe_duration_ms_rejects_when_wave_marker_corrupt() {
    // `RIFF...WAVy` (last byte different) — second arm of the `||`
    // catches this. Kills `||` ↔ `&&` boundary by exercising the
    // `&header[8..12] != b"WAVE"` branch.
    use std::io::Write;
    let mut buf = vec![0u8; 12];
    buf[0..4].copy_from_slice(b"RIFF");
    buf[4..8].copy_from_slice(&100u32.to_le_bytes());
    buf[8..12].copy_from_slice(b"WAVy");
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&buf).unwrap();
    tmp.flush().unwrap();
    let result = probe_duration_ms(tmp.path());
    assert!(matches!(result, Err(BackendError::Malformed(_))));
}

#[test]
fn probe_duration_ms_rejects_missing_fmt_chunk() {
    // RIFF/WAVE header followed by data-only chunk → byte_rate stays 0.
    // Kills line 229:18 `==` ↔ `!=` (returns Malformed when byte_rate==0).
    use std::io::Write;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&100u32.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&8u32.to_le_bytes());
    buf.extend(std::iter::repeat_n(0u8, 8));
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&buf).unwrap();
    tmp.flush().unwrap();
    let result = probe_duration_ms(tmp.path());
    assert!(matches!(result, Err(BackendError::Malformed(_))));
}

#[test]
fn probe_duration_ms_rejects_missing_data_chunk() {
    // WAV with fmt chunk but no data chunk → data_size stays 0.
    // Kills line 232:18 `==` ↔ `!=`.
    use std::io::Write;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&36u32.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&16000u32.to_le_bytes());
    buf.extend_from_slice(&32000u32.to_le_bytes()); // byte_rate
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    // No data chunk
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&buf).unwrap();
    tmp.flush().unwrap();
    let result = probe_duration_ms(tmp.path());
    assert!(matches!(result, Err(BackendError::Malformed(_))));
}

#[test]
fn probe_duration_ms_skips_unknown_chunks_to_find_data() {
    // Insert a `JUNK` chunk between `fmt ` and `data` — the loop's
    // wildcard arm must seek past it. Kills line 211:13 `delete match
    // arm b"fmt "` (without the fmt arm, byte_rate stays 0) and
    // line 218:13 `delete match arm b"data"` (without it, loop never
    // captures data_size and runs forever or breaks on EOF).
    use std::io::Write;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&100u32.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    // fmt chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&16000u32.to_le_bytes());
    buf.extend_from_slice(&32000u32.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    // JUNK chunk to skip
    buf.extend_from_slice(b"JUNK");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&32000u32.to_le_bytes());
    buf.extend(std::iter::repeat_n(0u8, 32000));
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&buf).unwrap();
    tmp.flush().unwrap();
    let ms = probe_duration_ms(tmp.path()).unwrap();
    assert_eq!(ms, 1000);
}

#[test]
fn probe_duration_ms_rejects_short_fmt_chunk() {
    // fmt chunk shorter than 12 bytes — the `if fmt.len() >= 12` guard
    // skips parsing byte_rate. Kills line 214:30 `>=` ↔ `<` by
    // ensuring an 8-byte fmt chunk leaves byte_rate=0 → Malformed.
    use std::io::Write;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&100u32.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&8u32.to_le_bytes()); // shorter than 12
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&32000u32.to_le_bytes());
    buf.extend(std::iter::repeat_n(0u8, 32000));
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&buf).unwrap();
    tmp.flush().unwrap();
    let result = probe_duration_ms(tmp.path());
    assert!(matches!(result, Err(BackendError::Malformed(_))));
}

// ── ffmpeg_path test ──────────────────────────────────────────────────────

#[test]
fn ffmpeg_path_includes_tools_dir_and_binary_name() {
    // Mutation `replace ffmpeg_path -> PathBuf with Default::default()`
    // returns an empty path. Real path joins tools_dir + "ffmpeg" or
    // "ffmpeg.exe" on Windows.
    let backend =
        WhisperXReplicateBackend::new("test-token", std::path::PathBuf::from("/opt/tools"));
    let path = backend.ffmpeg_path();
    let s = path.to_string_lossy();
    assert!(
        s.starts_with("/opt/tools"),
        "must start with tools_dir; got {s}"
    );
    let expected_name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    assert!(
        s.ends_with(expected_name),
        "must end with {expected_name}; got {s}"
    );
}

#[test]
fn build_predict_input_emits_expected_shape() {
    let input = build_predict_input("https://replicate.delivery/foo.wav", "en");
    assert_eq!(input["audio_file"], "https://replicate.delivery/foo.wav");
    assert_eq!(input["language"], "en");
    assert_eq!(input["align_output"], true);
    assert_eq!(input["diarization"], false);
    assert_eq!(input["batch_size"], 32);
    // No `initial_prompt` field — biasing Whisper with full lyrics
    // caused LM-fallback prompt-leakage on id=132 (#78). The phantom
    // cluster filter at text_reference_merge_phantom.rs is the chosen
    // remedy; this regression-asserts no one quietly re-adds the
    // prompt without explicit design.
    assert!(input.get("initial_prompt").is_none());
}

#[test]
fn parses_well_formed_segment_with_words() {
    let raw = serde_json::json!({
        "segments": [
            {
                "start": 1.5,
                "end": 3.2,
                "text": "Hello world",
                "words": [
                    {"word": "Hello", "start": 1.5, "end": 2.0, "score": 0.95},
                    {"word": "world", "start": 2.1, "end": 3.2, "score": 0.92},
                ]
            }
        ]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line.text, "Hello world");
    assert_eq!(line.start_ms, 1500);
    assert_eq!(line.end_ms, 3200);
    let words = line.words.as_ref().unwrap();
    assert_eq!(words.len(), 2);
    assert_eq!(words[0].text, "Hello");
    assert_eq!(words[0].start_ms, 1500);
}

#[test]
fn parses_segment_without_words_as_words_none() {
    let raw = serde_json::json!({
        "segments": [
            {"start": 0.0, "end": 5.0, "text": "no word timing"}
        ]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].words.is_none(), "missing words[] yields None");
}

#[test]
fn skips_empty_text_segments() {
    let raw = serde_json::json!({
        "segments": [
            {"start": 0.0, "end": 1.0, "text": ""},
            {"start": 1.0, "end": 2.0, "text": "  \n  "},
            {"start": 2.0, "end": 3.0, "text": "real line"}
        ]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "real line");
}

#[test]
fn rejects_missing_segments_field() {
    let raw = serde_json::json!({"foo": "bar"});
    let err = parse_output(&raw).unwrap_err();
    assert!(matches!(err, BackendError::Malformed(_)));
}

#[test]
fn drops_words_without_timestamps() {
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.0, "end": 2.0, "text": "two words",
            "words": [
                {"word": "two", "start": 0.0, "end": 1.0},
                {"word": "words", "start": null, "end": null},
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    let words = lines[0].words.as_ref().unwrap();
    assert_eq!(words.len(), 1, "untimestamped word filtered out");
    assert_eq!(words[0].text, "two");
}

#[test]
fn id_and_revision_are_stable() {
    let b =
        WhisperXReplicateBackend::new("test-token", std::path::PathBuf::from("/tmp/test-tools"));
    assert_eq!(b.id(), "whisperx-large-v3");
    assert_eq!(b.revision(), 1);
}

#[test]
fn capability_advertises_word_level_and_languages() {
    let b =
        WhisperXReplicateBackend::new("test-token", std::path::PathBuf::from("/tmp/test-tools"));
    let cap = b.capability();
    assert!(cap.word_level);
    assert!(cap.segment_level);
    assert!(cap.languages.contains(&"en"));
    assert!(cap.languages.contains(&"es"));
    assert!(cap.languages.contains(&"pt"));
}

#[test]
fn capability_max_audio_seconds_matches_prediction_timeout() {
    use crate::lyrics::replicate_client::PREDICTION_TIMEOUT;
    let b =
        WhisperXReplicateBackend::new("test-token", std::path::PathBuf::from("/tmp/test-tools"));
    let cap = b.capability();
    // max_audio_seconds must not exceed PREDICTION_TIMEOUT's seconds so
    // we never advertise handling durations we'd actually time out on.
    assert_eq!(
        cap.max_audio_seconds as u64,
        PREDICTION_TIMEOUT.as_secs(),
        "max_audio_seconds must equal PREDICTION_TIMEOUT seconds ({} s)",
        PREDICTION_TIMEOUT.as_secs()
    );
}

#[test]
fn all_untimestamped_words_yields_none() {
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.0, "end": 2.0, "text": "untimed words only",
            "words": [
                {"word": "untimed", "start": null, "end": null},
                {"word": "words", "start": null, "end": null},
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].words.is_none(),
        "all-untimestamped → None, not Some(vec![])"
    );
}

#[test]
fn default_align_opts_never_triggers_chunking() {
    let opts = AlignOpts::default();
    let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
    assert_eq!(trigger, u32::MAX);
}

#[test]
fn chunk_trigger_some_zero_means_always_chunk() {
    let opts = AlignOpts {
        chunk_trigger_seconds: Some(0),
    };
    let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
    assert_eq!(trigger, 0);
}

// ── parse_output: && filter (line 74 mutant) ──────────────────────────────
//
// Mutant: `&&` → `||` — would KEEP a word where start is Some but end is None,
// then unwrap the None end, yielding `(None_value * 1000) as u32 = 0` (or panic).
// The filter must require BOTH start AND end to be Some.

#[test]
fn parse_output_keeps_word_only_when_both_start_and_end_present() {
    // Word 1: start=Some, end=Some → kept
    // Word 2: start=Some, end=None → dropped (&&-semantics)
    // Word 3: start=None, end=Some → dropped (&&-semantics)
    // Under || mutant: words 2 and 3 would be kept, with end/start defaulting to 0.0
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.0, "end": 5.0, "text": "three words here",
            "words": [
                {"word": "three", "start": 0.1, "end": 0.9},
                {"word": "words", "start": 1.0, "end": null},
                {"word": "here",  "start": null, "end": 4.9},
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    let words = lines[0].words.as_ref().unwrap();
    // Only "three" has both start and end → only 1 word kept
    assert_eq!(
        words.len(),
        1,
        "only the word with both start AND end must be kept; got {words:?}"
    );
    assert_eq!(words[0].text, "three");
}

#[test]
fn parse_output_drops_word_with_only_start_none_end_some() {
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.0, "end": 2.0, "text": "hello",
            "words": [
                {"word": "hello", "start": null, "end": 2.0}
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    // start=None → filtered out by &&, words=None
    assert!(
        lines[0].words.is_none(),
        "word with start=null must be filtered (&&, not ||)"
    );
}

#[test]
fn parse_output_drops_word_with_start_some_end_none() {
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.0, "end": 2.0, "text": "hello",
            "words": [
                {"word": "hello", "start": 0.5, "end": null}
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    assert!(
        lines[0].words.is_none(),
        "word with end=null must be filtered (&&, not ||)"
    );
}

// ── parse_output: * 1000.0 conversion (line 78 mutant) ───────────────────
//
// Mutant A: `* 1000.0` → `+ 1000.0`: 1.5s would become 1001.5ms (truncated to 1001)
//           instead of 1500ms.
// Mutant B: `* 1000.0` → `/ 1000.0`: 1.5s would become 0.0015ms (truncated to 0)
//           instead of 1500ms.
// Both mutations produce wrong millisecond values for non-zero float inputs.

#[test]
fn parse_output_converts_seconds_to_milliseconds_correctly() {
    // start=1.5s → must become 1500ms (not 1001 or 0)
    // end=3.2s → must become 3200ms
    let raw = serde_json::json!({
        "segments": [{
            "start": 1.5,
            "end": 3.2,
            "text": "check timing",
            "words": [
                {"word": "check", "start": 1.5, "end": 2.3, "score": 0.9},
                {"word": "timing", "start": 2.4, "end": 3.2, "score": 0.9},
            ]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(lines[0].start_ms, 1500, "1.5s must become 1500ms (×1000)");
    assert_eq!(lines[0].end_ms, 3200, "3.2s must become 3200ms (×1000)");
    let words = lines[0].words.as_ref().unwrap();
    assert_eq!(
        words[0].start_ms, 1500,
        "word start 1.5s must become 1500ms"
    );
    assert_eq!(words[0].end_ms, 2300, "word end 2.3s must become 2300ms");
    assert_eq!(
        words[1].start_ms, 2400,
        "word start 2.4s must become 2400ms"
    );
    assert_eq!(words[1].end_ms, 3200, "word end 3.2s must become 3200ms");
}

#[test]
fn parse_output_ms_conversion_distinguishes_from_addition() {
    // At start=0.5s: correct=500ms, +1000 mutant=1000ms, /1000 mutant=0ms.
    let raw = serde_json::json!({
        "segments": [{
            "start": 0.5, "end": 0.9, "text": "x",
            "words": [{"word": "x", "start": 0.5, "end": 0.9}]
        }]
    });
    let lines = parse_output(&raw).unwrap();
    let words = lines[0].words.as_ref().unwrap();
    assert_eq!(
        words[0].start_ms, 500,
        "0.5s * 1000 = 500ms (not 1000 from +1000, not 0 from /1000)"
    );
    assert_eq!(words[0].end_ms, 900, "0.9s * 1000 = 900ms");
}

#[test]
fn parse_output_ms_conversion_large_value() {
    // start=120.5s → 120500ms. Under +1000 mutant: 1120ms (way off).
    let raw = serde_json::json!({
        "segments": [{
            "start": 120.5, "end": 122.0, "text": "late line"
        }]
    });
    let lines = parse_output(&raw).unwrap();
    assert_eq!(
        lines[0].start_ms, 120500,
        "120.5s must become 120500ms (× 1000)"
    );
    assert_eq!(lines[0].end_ms, 122000, "122.0s must become 122000ms");
}

#[tokio::test]
async fn replicate_to_backend_err_maps_reqwest_timeout_to_timeout() {
    use std::time::Duration;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let err = client
        .get(server.uri())
        .send()
        .await
        .expect_err("expected timeout");
    assert!(err.is_timeout(), "precondition: reqwest reports timeout");
    let mapped = replicate_to_backend_err(ReplicateError::Http(err));
    assert!(
        matches!(mapped, BackendError::Timeout(_)),
        "is_timeout() must map to Timeout, got: {mapped:?}"
    );
}

#[tokio::test]
async fn replicate_to_backend_err_maps_non_timeout_reqwest_to_transport() {
    // DNS failure on .invalid (RFC 6761 reserved TLD) — not a timeout.
    let client = reqwest::Client::builder().build().unwrap();
    let err = client
        .get("http://nonexistent.invalid/")
        .send()
        .await
        .expect_err("expected DNS error");
    assert!(!err.is_timeout(), "precondition: DNS error is not timeout");
    let mapped = replicate_to_backend_err(ReplicateError::Http(err));
    assert!(
        matches!(mapped, BackendError::Transport(_)),
        "non-timeout must map to Transport, got: {mapped:?}"
    );
}
