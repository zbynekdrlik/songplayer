//! AlignmentBackend trait — pluggable ASR/alignment engine abstraction.
//!
//! Initial impl: WhisperXReplicateBackend (see whisperx_replicate.rs).
//! Future impls (Parakeet, CrisperWhisper, AudioShake, VibeVoice) plug
//! in here without rewriting the orchestrator.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedLine {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    /// `None` for segment-only backends (VibeVoice etc.). Renderer falls
    /// back to line-level highlighting when None — never synthesize evenly
    /// distributed word timings (per `feedback_no_even_distribution.md`).
    pub words: Option<Vec<AlignedWord>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedTrack {
    pub lines: Vec<AlignedLine>,
    /// e.g. "whisperx-large-v3@rev1"
    pub provenance: String,
    /// Self-reported by backend. NOT a quality gate — just metadata.
    pub raw_confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub struct AlignmentCapability {
    pub word_level: bool,
    pub segment_level: bool,
    pub max_audio_seconds: u32,
    /// BCP-47 language codes the backend supports.
    pub languages: &'static [&'static str],
    pub takes_reference_text: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AlignOpts {
    /// Optional override for the chunking trigger threshold (seconds).
    /// `None` = backend default. `Some(0)` = always chunk. `Some(u32::MAX)` = never chunk.
    pub chunk_trigger_seconds: Option<u32>,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend transport error: {0}")]
    Transport(String),
    #[error("backend rejected request: {0}")]
    Rejected(String),
    #[error("backend timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("backend output malformed: {0}")]
    Malformed(String),
    #[error("backend rate-limited: {0}")]
    RateLimit(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait AlignmentBackend: Send + Sync {
    /// Stable identifier persisted in DB & JSON. e.g. "whisperx-large-v3".
    fn id(&self) -> &'static str;

    /// Bumped per-backend when output contract changes. Use with
    /// LYRICS_PIPELINE_VERSION for stale-bucket re-queue logic.
    fn revision(&self) -> u32;

    /// What this backend can do.
    fn capability(&self) -> AlignmentCapability;

    /// Transcribe + align. `vocal_wav_path` MUST be the Mel-Roformer +
    /// anvuew dereverb stem (NOT raw mix). `language` is BCP-47.
    async fn align(
        &self,
        vocal_wav_path: &Path,
        reference_text: Option<&str>,
        language: &str,
        opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// MockBackend: trivial impl proving the trait is callable.
    struct MockBackend;

    #[async_trait]
    impl AlignmentBackend for MockBackend {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn revision(&self) -> u32 {
            1
        }
        fn capability(&self) -> AlignmentCapability {
            AlignmentCapability {
                word_level: true,
                segment_level: true,
                max_audio_seconds: 600,
                languages: &["en"],
                takes_reference_text: false,
            }
        }
        async fn align(
            &self,
            _wav: &Path,
            _ref_text: Option<&str>,
            _lang: &str,
            _opts: &AlignOpts,
        ) -> Result<AlignedTrack, BackendError> {
            Ok(AlignedTrack {
                lines: vec![AlignedLine {
                    text: "hello world".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    words: Some(vec![
                        AlignedWord {
                            text: "hello".into(),
                            start_ms: 0,
                            end_ms: 500,
                            confidence: 0.9,
                        },
                        AlignedWord {
                            text: "world".into(),
                            start_ms: 500,
                            end_ms: 1000,
                            confidence: 0.9,
                        },
                    ]),
                }],
                provenance: "mock@rev1".into(),
                raw_confidence: 0.9,
            })
        }
    }

    #[tokio::test]
    async fn mock_backend_returns_aligned_track() {
        let b = MockBackend;
        let r = b
            .align(
                &PathBuf::from("/tmp/test.wav"),
                None,
                "en",
                &AlignOpts::default(),
            )
            .await
            .unwrap();
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "hello world");
        assert_eq!(r.lines[0].words.as_ref().unwrap().len(), 2);
        assert_eq!(r.provenance, "mock@rev1");
    }

    #[test]
    fn aligned_line_words_can_be_none() {
        let line = AlignedLine {
            text: "segment-only output".into(),
            start_ms: 0,
            end_ms: 5000,
            words: None, // segment-only backends like VibeVoice
        };
        assert!(line.words.is_none());
    }

    #[test]
    fn capability_can_advertise_no_word_level() {
        let cap = AlignmentCapability {
            word_level: false,
            segment_level: true,
            max_audio_seconds: 3600,
            languages: &["en"],
            takes_reference_text: false,
        };
        assert!(!cap.word_level);
        assert!(cap.segment_level);
    }
}
