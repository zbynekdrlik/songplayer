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

/// Build the JSON input payload for a Replicate WhisperX prediction.
/// Extracted for unit testing.
fn build_predict_input(audio_url: &str, language: &str) -> Value {
    serde_json::json!({
        "audio_file": audio_url,
        "language": language,
        "align_output": true,
        "diarization": false,
        "batch_size": 32,
    })
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
        }
    }

    #[cfg_attr(test, mutants::skip)]
    async fn align(
        &self,
        vocal_wav_path: &Path,
        language: &str,
        opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError> {
        let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
        let duration_ms = if opts.chunk_trigger_seconds.is_some() {
            probe_duration_ms(vocal_wav_path)?
        } else {
            0
        };

        let lines = if duration_ms / 1000 > trigger as u64 {
            align_chunked(self, vocal_wav_path, language, duration_ms).await?
        } else {
            let url = self
                .client
                .upload_file(vocal_wav_path)
                .await
                .map_err(replicate_to_backend_err)?;
            let input = build_predict_input(&url, language);
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
        let input = build_predict_input(&url, language);
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
        all.extend(merge_chunk_lines(chunk_lines, plan));
    }

    all.sort_by_key(|l| l.start_ms);
    Ok(all)
}

/// Offset chunk-local timings to global timings and drop overlap-region
/// lines that the previous chunk already produced. Pure function so the
/// chunk-merge invariant can be unit-tested without ffmpeg or HTTP.
///
/// Chunks are produced with `CHUNK_OVERLAP_MS` overlap so faster-whisper
/// VAD doesn't clip a sentence at the chunk boundary. The overlap region
/// of chunk N is identical audio to the tail of chunk N-1 — whisperx
/// transcribes both, sometimes producing slightly different text for the
/// same audio (see comment in caller). Ownership-based dedup (drop chunk
/// N's lines whose global start falls inside the overlap with N-1)
/// catches this regardless of text-level differences.
fn merge_chunk_lines(
    chunk_lines: Vec<AlignedLine>,
    plan: &crate::lyrics::audio_chunking::ChunkPlan,
) -> Vec<AlignedLine> {
    let offset = plan.start_ms as u32;
    let drop_below_ms = if plan.idx == 0 {
        0
    } else {
        (plan.start_ms + CHUNK_OVERLAP_MS) as u32
    };
    let mut out: Vec<AlignedLine> = Vec::with_capacity(chunk_lines.len());
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
        out.push(line);
    }
    out
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
#[path = "whisperx_replicate_tests.rs"]
mod tests;
