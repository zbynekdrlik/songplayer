//! WhisperXReplicateBackend — AlignmentBackend impl for victor-upmeet/whisperx
//! on Replicate (Whisper-large-v3 + wav2vec2-CTC alignment).
//!
//! Verified during design phase (2026-04-28) on 3 yt_subs ground-truth songs;
//! WhisperX scored 18 sub-1s line matches on the 11.8-min "There Is A King".

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::lyrics::backend::{
    AlignOpts, AlignedLine, AlignedTrack, AlignedWord, AlignmentBackend, AlignmentCapability,
    BackendError,
};
use crate::lyrics::replicate_client::{ReplicateClient, ReplicateError};

/// Pinned version hash discovered at plan-write time (April 2026).
/// Update when Replicate publishes a new wrapper version that we choose
/// to upgrade to. Bumped together with `revision()` below.
pub const WHISPERX_VERSION: &str = "84d2ad2d61945af5e7517a9efaee9c12d3a9d9a3";

pub struct WhisperXReplicateBackend {
    client: ReplicateClient,
}

impl WhisperXReplicateBackend {
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            client: ReplicateClient::new(api_token),
        }
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
        let words = if s.words.is_empty() {
            None
        } else {
            Some(
                s.words
                    .iter()
                    .filter(|w| w.start.is_some() && w.end.is_some())
                    .map(|w| AlignedWord {
                        text: w.word.trim().to_string(),
                        start_ms: (w.start.unwrap_or(0.0) * 1000.0) as u32,
                        end_ms: (w.end.unwrap_or(0.0) * 1000.0) as u32,
                        confidence: w.score.unwrap_or(0.9) as f32,
                    })
                    .collect(),
            )
        };
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
            // WhisperX handles long-form natively via faster-whisper VAD chunking.
            // Songs > this duration would need chunking trigger (Task A.5).
            max_audio_seconds: 3_600,
            languages: &["en", "es", "pt", "fr", "de", "it", "nl", "pl", "ru", "uk"],
            takes_reference_text: false,
        }
    }

    async fn align(
        &self,
        vocal_wav_path: &Path,
        _reference_text: Option<&str>,
        language: &str,
        _opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError> {
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

        let lines = parse_output(&output)?;
        Ok(AlignedTrack {
            lines,
            provenance: format!("{}@rev{}", self.id(), self.revision()),
            raw_confidence: 0.9,
        })
    }
}

fn replicate_to_backend_err(e: ReplicateError) -> BackendError {
    use ReplicateError::*;
    match e {
        Http(err) => BackendError::Transport(err.to_string()),
        Io(err) => BackendError::Io(err),
        ApiError { status, body } => BackendError::Rejected(format!("HTTP {status}: {body}")),
        RateLimited(n) => BackendError::RateLimit(format!("after {n} attempts")),
        PredictionFailed(s) => BackendError::Rejected(s),
        Timeout => BackendError::Timeout(std::time::Duration::from_secs(1800)),
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
        let b = WhisperXReplicateBackend::new("test-token");
        assert_eq!(b.id(), "whisperx-large-v3");
        assert_eq!(b.revision(), 1);
    }

    #[test]
    fn capability_advertises_word_level_and_languages() {
        let b = WhisperXReplicateBackend::new("test-token");
        let cap = b.capability();
        assert!(cap.word_level);
        assert!(cap.segment_level);
        assert!(cap.languages.contains(&"en"));
        assert!(cap.languages.contains(&"es"));
        assert!(cap.languages.contains(&"pt"));
    }
}
