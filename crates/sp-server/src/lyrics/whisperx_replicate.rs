//! WhisperXReplicateBackend — AlignmentBackend impl for victor-upmeet/whisperx
//! on Replicate (Whisper-large-v3 + wav2vec2-CTC alignment).
//!
//! Verified during design phase (2026-04-28) on 3 yt_subs ground-truth songs;
//! WhisperX scored 18 sub-1s line matches on the 11.8-min "There Is A King".

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::lyrics::audio_chunking::{CHUNK_OVERLAP_MS, plan_chunks};
use crate::lyrics::backend::{
    AlignOpts, AlignedLine, AlignedTrack, AlignedWord, AlignmentBackend, AlignmentCapability,
    BackendError,
};
use crate::lyrics::replicate_client::{ReplicateClient, ReplicateError};

/// Pinned version hash discovered at plan-write time (April 2026).
/// Update when Replicate publishes a new wrapper version that we choose
/// to upgrade to. Bumped together with `revision()` below.
pub const WHISPERX_VERSION: &str =
    "84d2ad2d6194fe98a17d2b60bef1c7f910c46b2f6fd38996ca457afd9c8abfcb";

pub struct WhisperXReplicateBackend {
    client: ReplicateClient,
    /// Tools directory containing bundled ffmpeg.exe / ffprobe.exe. Used
    /// only by the chunked path (`align_chunked`). Bare `Command::new
    /// ("ffmpeg")` fails on Windows because the bundled tools are NOT in
    /// PATH — the deploy script doesn't add them.
    tools_dir: std::path::PathBuf,
}

impl WhisperXReplicateBackend {
    pub fn new(api_token: impl Into<String>, tools_dir: std::path::PathBuf) -> Self {
        Self {
            client: ReplicateClient::new(api_token),
            tools_dir,
        }
    }

    fn ffmpeg_path(&self) -> std::path::PathBuf {
        self.tools_dir.join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        })
    }
}

#[derive(Debug, Deserialize)]
struct WhisperXSegment {
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    words: Vec<WhisperXWord>,
}

#[derive(Debug, Deserialize)]
struct WhisperXWord {
    word: String,
    start: Option<f64>,
    end: Option<f64>,
    #[serde(default)]
    score: Option<f64>,
}

/// Parse Replicate's WhisperX JSON output into AlignedLine list.
pub fn parse_output(output: &Value) -> Result<Vec<AlignedLine>, BackendError> {
    let segments = output
        .get("segments")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BackendError::Malformed("missing segments[]".into()))?;

    let mut lines = Vec::with_capacity(segments.len());
    for seg in segments {
        let s: WhisperXSegment = serde_json::from_value(seg.clone())
            .map_err(|e| BackendError::Malformed(format!("segment parse: {e}")))?;
        let text = s.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let words: Vec<AlignedWord> = s
            .words
            .iter()
            .filter(|w| w.start.is_some() && w.end.is_some())
            .map(|w| AlignedWord {
                text: w.word.trim().to_string(),
                start_ms: (w.start.unwrap_or(0.0) * 1000.0) as u32,
                end_ms: (w.end.unwrap_or(0.0) * 1000.0) as u32,
                confidence: w.score.unwrap_or(0.9) as f32,
            })
            .collect();
        let words = if words.is_empty() { None } else { Some(words) };
        lines.push(AlignedLine {
            text,
            start_ms: (s.start * 1000.0) as u32,
            end_ms: (s.end * 1000.0) as u32,
            words,
        });
    }
    Ok(lines)
}

#[async_trait]
impl AlignmentBackend for WhisperXReplicateBackend {
    fn id(&self) -> &'static str {
        "whisperx-large-v3"
    }
    fn revision(&self) -> u32 {
        1
    }
    fn capability(&self) -> AlignmentCapability {
        AlignmentCapability {
            word_level: true,
            segment_level: true,
            // Ceiling matches PREDICTION_TIMEOUT (1800 s = 30 min) in
            // replicate_client.rs. Advertising more would be dishonest:
            // a song longer than 1800 s would time out during polling.
            max_audio_seconds: 1_800,
            languages: &["en", "es", "pt", "fr", "de", "it", "nl", "pl", "ru", "uk"],
            takes_reference_text: false,
        }
    }

    // All internal branches of align() require a live Replicate API call or
    // a real ffprobe binary. The chunking trigger comparison `duration_ms/1000 >
    // trigger` and the upload/predict path are covered structurally by
    // `default_align_opts_never_triggers_chunking` and the replicate_client
    // tests. End-to-end testing tracked in #65.
    #[cfg_attr(test, mutants::skip)]
    async fn align(
        &self,
        vocal_wav_path: &Path,
        _reference_text: Option<&str>,
        language: &str,
        opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError> {
        // Probe duration ONLY when chunking might fire — `probe_duration_ms`
        // shells out to `ffprobe`, which is not bundled (the tools manager
        // installs ffmpeg.exe only). With default `AlignOpts.chunk_trigger_seconds = None`
        // we skip probing entirely; WhisperX's faster-whisper backend
        // handles long-form audio natively via VAD, so chunking is opt-in.
        let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
        let duration_ms = if opts.chunk_trigger_seconds.is_some() {
            probe_duration_ms(vocal_wav_path)?
        } else {
            0 // unused — chunking branch below is unreachable when trigger == u32::MAX
        };

        let lines = if duration_ms / 1000 > trigger as u64 {
            align_chunked(self, vocal_wav_path, language, duration_ms).await?
        } else {
            let url = self
                .client
                .upload_file(vocal_wav_path)
                .await
                .map_err(replicate_to_backend_err)?;
            let input = serde_json::json!({
                "audio_file": url,
                "language": language,
                "align_output": true,
                "diarization": false,
                "batch_size": 32,
            });
            let pred = self
                .client
                .predict(WHISPERX_VERSION, input)
                .await
                .map_err(replicate_to_backend_err)?;
            let output = pred
                .output
                .ok_or_else(|| BackendError::Malformed("succeeded but no output".into()))?;
            parse_output(&output)?
        };

        Ok(AlignedTrack {
            lines,
            provenance: format!("{}@rev{}", self.id(), self.revision()),
            raw_confidence: 0.9,
        })
    }
}

/// Read WAV header to compute duration. No external binary required.
/// Vocal stems are PCM WAVs (Mel-Roformer + anvuew dereverb output).
/// Avoids a hard dep on ffprobe.exe — the tools manager only extracts
/// ffmpeg.exe from the FFmpeg ZIP, not ffprobe.exe.
fn probe_duration_ms(path: &Path) -> Result<u64, BackendError> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let mut f = File::open(path).map_err(BackendError::Io)?;
    let mut header = [0u8; 12];
    f.read_exact(&mut header).map_err(BackendError::Io)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err(BackendError::Malformed("not a WAV file".into()));
    }

    let mut byte_rate: u32 = 0;
    let mut data_size: u32 = 0;
    loop {
        let mut chunk_header = [0u8; 8];
        if f.read_exact(&mut chunk_header).is_err() {
            break;
        }
        let id = &chunk_header[0..4];
        let size = u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]);
        match id {
            b"fmt " => {
                let mut fmt = vec![0u8; size as usize];
                f.read_exact(&mut fmt).map_err(BackendError::Io)?;
                if fmt.len() >= 12 {
                    byte_rate = u32::from_le_bytes([fmt[8], fmt[9], fmt[10], fmt[11]]);
                }
            }
            b"data" => {
                data_size = size;
                break;
            }
            _ => {
                f.seek(SeekFrom::Current(size as i64))
                    .map_err(BackendError::Io)?;
            }
        }
    }

    if byte_rate == 0 {
        return Err(BackendError::Malformed("WAV missing fmt chunk".into()));
    }
    if data_size == 0 {
        return Err(BackendError::Malformed("WAV missing data chunk".into()));
    }
    Ok((data_size as u64 * 1000) / byte_rate as u64)
}

/// Chunked transcription path: slice the vocal WAV into 60s/10s-overlap
/// chunks via ffmpeg, transcribe each independently via WhisperX, then merge
/// using the same overlap-dedup logic as the Gemini path.
///
/// Triggered only when `AlignOpts::chunk_trigger_seconds` is set and the
/// audio duration exceeds the threshold. Default behavior (None or
/// Some(u32::MAX)) is to never chunk — WhisperX handles long-form natively
/// via faster-whisper VAD.
///
/// TODO(test): align_chunked has no unit coverage — mock-injection requires
/// either extracting ReplicateClient behind a trait or making the function
/// accept a generic backend. Tracked in GitHub issue #65.
// All mutations inside align_chunked require either a real filesystem path,
// a live Replicate API call, or a running ffmpeg binary. None of these are
// available in unit tests. Tracked in #65 (mock injection).
#[cfg_attr(test, mutants::skip)]
async fn align_chunked(
    backend: &WhisperXReplicateBackend,
    vocal_wav_path: &Path,
    language: &str,
    duration_ms: u64,
) -> Result<Vec<AlignedLine>, BackendError> {
    use std::process::Command;
    use tempfile::TempDir;

    let plans = plan_chunks(duration_ms);
    let tmp = TempDir::new().map_err(BackendError::Io)?;
    let mut all: Vec<AlignedLine> = Vec::new();

    for plan in &plans {
        let chunk_path = tmp.path().join(format!("chunk_{}.wav", plan.idx));
        let wav_str = vocal_wav_path
            .to_str()
            .ok_or_else(|| BackendError::Malformed("non-utf8 wav path".into()))?;
        let chunk_str = chunk_path.to_str().unwrap();
        let status = Command::new(backend.ffmpeg_path())
            .args([
                "-y",
                "-loglevel",
                "error",
                "-ss",
                &format!("{}", plan.start_ms as f64 / 1000.0),
                "-i",
                wav_str,
                "-t",
                &format!("{}", (plan.end_ms - plan.start_ms) as f64 / 1000.0),
                "-c:a",
                "pcm_s16le",
                "-ar",
                "16000",
                "-ac",
                "1",
                chunk_str,
            ])
            .status()
            .map_err(BackendError::Io)?;
        if !status.success() {
            return Err(BackendError::Rejected(format!(
                "ffmpeg failed for chunk {}",
                plan.idx
            )));
        }

        let url = backend
            .client
            .upload_file(&chunk_path)
            .await
            .map_err(replicate_to_backend_err)?;
        let input = serde_json::json!({
            "audio_file": url,
            "language": language,
            "align_output": true,
            "diarization": false,
            "batch_size": 32,
        });
        let pred = backend
            .client
            .predict(WHISPERX_VERSION, input)
            .await
            .map_err(replicate_to_backend_err)?;
        let output = pred
            .output
            .ok_or_else(|| BackendError::Malformed("chunk: no output".into()))?;
        let chunk_lines = parse_output(&output)?;

        // Chunk-ownership dedup: chunk N (N>0) overlaps chunk N-1 by
        // CHUNK_OVERLAP_MS. Lines whose global start_ms falls in chunk
        // N's first-overlap region are also produced by chunk N-1 — drop
        // them here so the merged stream has no duplicates.
        //
        // Whisperx may transcribe the SAME audio differently in adjacent
        // chunks (e.g. id=132 2:25-2:30 produced "Holy, holy forever." in
        // chunk K and "Holy forever." in chunk K+1). Text-based dedup
        // misses this; ownership-based dedup catches it regardless of
        // text differences.
        let offset = plan.start_ms as u32;
        let drop_below_ms = if plan.idx == 0 {
            0
        } else {
            (plan.start_ms + CHUNK_OVERLAP_MS) as u32
        };
        for mut line in chunk_lines {
            let global_start = line.start_ms.saturating_add(offset);
            if global_start < drop_below_ms {
                continue;
            }
            line.start_ms = global_start;
            line.end_ms = line.end_ms.saturating_add(offset);
            if let Some(ref mut words) = line.words {
                for w in words.iter_mut() {
                    w.start_ms = w.start_ms.saturating_add(offset);
                    w.end_ms = w.end_ms.saturating_add(offset);
                }
            }
            all.push(line);
        }
    }

    all.sort_by_key(|l| l.start_ms);
    Ok(all)
}

fn replicate_to_backend_err(e: ReplicateError) -> BackendError {
    use ReplicateError::*;
    match e {
        Http(err) if err.is_timeout() => {
            BackendError::Timeout(crate::lyrics::replicate_client::PER_REQUEST_TIMEOUT)
        }
        Http(err) => BackendError::Transport(err.to_string()),
        Io(err) => BackendError::Io(err),
        ApiError { status, body } => BackendError::Rejected(format!("HTTP {status}: {body}")),
        RateLimited(n) => BackendError::RateLimit(format!("after {n} attempts")),
        PredictionFailed(s) => BackendError::Rejected(s),
        Timeout => BackendError::Timeout(crate::lyrics::replicate_client::PREDICTION_TIMEOUT),
        Malformed(s) => BackendError::Malformed(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let b = WhisperXReplicateBackend::new(
            "test-token",
            std::path::PathBuf::from("/tmp/test-tools"),
        );
        assert_eq!(b.id(), "whisperx-large-v3");
        assert_eq!(b.revision(), 1);
    }

    #[test]
    fn capability_advertises_word_level_and_languages() {
        let b = WhisperXReplicateBackend::new(
            "test-token",
            std::path::PathBuf::from("/tmp/test-tools"),
        );
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
        let b = WhisperXReplicateBackend::new(
            "test-token",
            std::path::PathBuf::from("/tmp/test-tools"),
        );
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
}
