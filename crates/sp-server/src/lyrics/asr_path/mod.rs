//! ASR alignment path — AssemblyAI U3-Pro transcription + deterministic
//! silence-gap line splitting, for songs the whisperx gate rejects (no timed
//! text source). AAI is the content source of truth; reference lyrics
//! (genius/lrclib) are HELPER-only — fed to AAI as `keyterms_prompt` to bias
//! recognition, never to add/drop lines.
//!
//! Pipeline: AAI transcribe → silence-gap split → OPTIONAL drop-safe Claude
//! regroup (`regroup.rs`: operates on line INDICES, reconstructs merged text
//! from the covered AAI lines, falls back to the raw split on any failure —
//! never the old word-level merge that dropped choruses) → LED-wall line-width
//! split (`line_splitter`) → `words: None` (line-level only).
//!
//! See `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`
//! (the original word-index Claude-merge in that spec is superseded; only the
//! index-level regroup survives).

pub mod aai_backend;
pub mod fallback;
pub mod regroup;
pub mod sanitize;

use std::path::Path;

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::{AaiBackend, AaiError, AaiTranscript};

/// DB settings key that stores the AssemblyAI API token. Read per-song in
/// the worker so operators can configure without a restart.
pub const ASSEMBLYAI_API_KEY_SETTING: &str = "assemblyai_api_key";

/// Source label stamped on rows produced by this path.
pub const SOURCE_ASR: &str = "asr:aai-u3-pro";

/// Per-song audit snapshot, written by the worker as a JSON sidecar.
#[derive(Debug, serde::Serialize)]
pub struct AsrAudit {
    pub outcome: &'static str, // "lines" | "quarantine"
    pub source_label: Option<&'static str>,
    pub quarantine_reason: Option<&'static str>,
    pub aai_word_count: usize,
    pub line_count: usize,
}

#[derive(Debug)]
pub struct AsrResult {
    pub output: AsrOutput,
    pub audit: AsrAudit,
}

#[derive(Debug)]
pub enum AsrOutput {
    /// Transcribed + split into lines. Persisted with source = SOURCE_ASR.
    Lines {
        lines: Vec<LyricsLine>,
        source: &'static str,
    },
    /// No usable output — empty transcript or no lines after splitting. Worker
    /// quarantines the row as `asr_gap`.
    Quarantine { reason: &'static str },
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    /// HTTP 429 from AAI — quota cap reached. Surfaced to operator; row stays
    /// unprocessed for retry.
    #[error("AAI quota exhausted — surface to user, no row write")]
    QuotaExhausted,
    /// Underlying AAI failure — transport / parse / remote error.
    #[error("AAI transcription failed: {0}")]
    Aai(#[from] AaiError),
}

/// `keyterms` biases AAI recognition toward expected phrases (the gathered
/// reference lyric lines — genius/lrclib). It NEVER adds or drops words; it
/// only helps the model resolve words already in the audio. Pass `&[]` for
/// none. This is the only role the reference text plays in the lean path —
/// a helper input to the one model, not an authoritative line source.
///
/// `chat` is an optional Claude client for the post-split regroup pass. When
/// `Some`, Claude merges over-split AAI lines into singable lines guided by
/// `reference_lines` phrasing. The pass is drop-safe: if Claude's grouping
/// fails validation, the raw split is returned unchanged. Pass `None` to skip.
///
/// `reference_lines` are the gathered reference lyric lines (genius/lrclib)
/// used to guide Claude's phrasing. Reused from `keyterms` — pass the same
/// slice for both. Ignored when `chat` is `None`.
pub async fn run(
    aai: &AaiBackend,
    chat: Option<&dyn crate::lyrics::asr_path::regroup::RegroupChat>,
    audio_path: &Path,
    keyterms: &[String],
    reference_lines: &[String],
) -> Result<AsrResult, AsrError> {
    let transcript: AaiTranscript = match aai.transcribe(audio_path, keyterms).await {
        Ok(t) => t,
        Err(AaiError::QuotaExhausted) => return Err(AsrError::QuotaExhausted),
        Err(e) => return Err(AsrError::Aai(e)),
    };
    let aai_word_count = transcript.words.len();

    // Diagnostic: dump the full AAI word stream (idx:text@start-end) so the
    // transcript is inspectable when investigating phrasing/coverage.
    tracing::info!(
        word_count = aai_word_count,
        words = %transcript
            .words
            .iter()
            .enumerate()
            .map(|(i, w)| format!("{i}:{}@{}-{}", w.text, w.start_ms, w.end_ms))
            .collect::<Vec<_>>()
            .join(" "),
        "asr_path: AAI transcript dump"
    );

    if transcript.words.is_empty() {
        return Ok(AsrResult {
            output: AsrOutput::Quarantine {
                reason: "empty_transcript",
            },
            audit: AsrAudit {
                outcome: "quarantine",
                source_label: None,
                quarantine_reason: Some("empty_transcript"),
                aai_word_count: 0,
                line_count: 0,
            },
        });
    }

    let lines = fallback::split_on_silence(&transcript.words);
    if lines.is_empty() {
        return Ok(AsrResult {
            output: AsrOutput::Quarantine {
                reason: "empty_split",
            },
            audit: AsrAudit {
                outcome: "quarantine",
                source_label: None,
                quarantine_reason: Some("empty_split"),
                aai_word_count,
                line_count: 0,
            },
        });
    }

    // Optional Claude regroup pass: merges over-split lines into singable
    // lines guided by reference phrasing. Drop-safe — falls back to raw split
    // on any validation failure. line_count is computed AFTER regroup so the
    // audit reflects the final output shape.
    let lines = match chat {
        Some(c) => crate::lyrics::asr_path::regroup::regroup(c, &lines, reference_lines).await,
        None => lines,
    };

    // Enforce LED-wall line width: split any line longer than max_chars at the
    // best sentence/comma/word boundary (same logic + default the whisperx flow
    // uses via line_splitter::split_track). Keeps lines ≤ ~32 chars so long
    // phrases like "His name will bring complete breakthrough" wrap cleanly.
    let lines = crate::lyrics::line_splitter::split_lyrics_lines(
        lines,
        crate::lyrics::line_splitter::SplitConfig::default(),
    );

    let line_count = lines.len();
    Ok(AsrResult {
        output: AsrOutput::Lines {
            lines,
            source: SOURCE_ASR,
        },
        audit: AsrAudit {
            outcome: "lines",
            source_label: Some(SOURCE_ASR),
            quarantine_reason: None,
            aai_word_count,
            line_count,
        },
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
