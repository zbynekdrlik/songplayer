//! ASR alignment path — runs AssemblyAI U3-Pro + Claude-merge for songs
//! whose `gather_sources` returns ONLY untimed text candidates (genius,
//! lrclib-untimed, etc.). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`.

pub mod aai_backend;
pub mod claude_merge;
pub mod fallback;
pub mod merge_prompt;
pub mod resolver;
pub mod sanitize;

use std::path::Path;

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::{AaiBackend, AaiError, AaiTranscript};
use crate::lyrics::asr_path::claude_merge::{MergeChat, MergeError, merge};
use crate::lyrics::asr_path::merge_prompt::ClaudeMergeInput;
use crate::lyrics::asr_path::resolver::{ResolverError, resolve};
use crate::lyrics::tier1::CandidateText;

/// DB settings key that stores the AssemblyAI API token. Read per-song in
/// the worker so operators can configure without a restart. Same pattern
/// as `replicate_api_token`.
pub const ASSEMBLYAI_API_KEY_SETTING: &str = "assemblyai_api_key";

pub const SOURCE_MERGED: &str = "asr:aai-u3-pro+claude-merge";
pub const SOURCE_FALLBACK: &str = "asr:aai-u3-pro";

/// Per-song audit snapshot. Worker writes one JSON sidecar per processed song
/// to `{cache_dir}/{youtube_id}_asr_path_audit.json`. Captures the decision
/// trail (which path fired, whether Claude disagreed, the fallback reason)
/// without parsing tracing logs.
#[derive(Debug, serde::Serialize)]
pub struct AsrAudit {
    pub outcome: &'static str, // "merged" | "fallback" | "quarantine"
    pub source_label: Option<&'static str>, // None for quarantine
    pub quarantine_reason: Option<&'static str>,
    pub aai_word_count: usize,
    pub claude_disagreement: Option<bool>,
    pub claude_notes: Option<String>,
    pub claude_line_count: Option<usize>,
    pub fallback_reason: Option<&'static str>, // "transport" | "rejected" | "disagreement_or_empty" | "resolver_error" | None
}

/// Combined return value from `run`. Carries both the processing outcome and
/// a structured audit record that the worker writes as a JSON sidecar.
#[derive(Debug)]
pub struct AsrResult {
    pub output: AsrOutput,
    pub audit: AsrAudit,
}

#[derive(Debug)]
pub enum AsrOutput {
    /// Claude-merge produced usable line splits referencing AAI words.
    /// Persisted with source = "asr:aai-u3-pro+claude-merge".
    Merged {
        lines: Vec<LyricsLine>,
        source: &'static str,
    },
    /// Claude declined the merge (disagreement / malformed / refusal) or the
    /// resolver rejected its output. Lines come from raw AAI silence-gap split.
    /// Persisted with source = "asr:aai-u3-pro".
    Fallback {
        lines: Vec<LyricsLine>,
        source: &'static str,
    },
    /// No usable output — empty transcript or empty fallback. Worker quarantines
    /// the row as `asr_gap`. The reason string is logged + persisted in the
    /// audit trail.
    Quarantine { reason: &'static str },
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    /// HTTP 429 from AAI — quota cap reached. Surfaced to operator via
    /// dashboard event; the song stays unprocessed for retry on next worker tick.
    #[error("AAI quota exhausted — surface to user, no row write")]
    QuotaExhausted,
    /// Underlying AAI failure — transport / parse / remote `status: error`.
    #[error("AAI transcription failed: {0}")]
    Aai(#[from] AaiError),
    /// Claude HTTP transport failed (network / 5xx). Orchestrator falls back
    /// to raw AAI silence-gap split.
    #[error("Claude merge transport failed: {0}")]
    MergeTransport(String),
    /// No candidate with non-empty lines — caller should not have entered
    /// asr_path. Indicates a routing bug upstream.
    #[error("no usable untimed text candidate found")]
    NoCandidate,
}

/// Pick the best untimed text candidate. Priority: genius > lrclib > others.
/// "Best" within a priority tier means the candidate with the most lines.
pub fn pick_untimed_candidate(candidates: &[CandidateText]) -> Option<&CandidateText> {
    fn rank(source: &str) -> u8 {
        match source {
            s if s.contains("genius") => 0,
            s if s.contains("lrclib") => 1,
            _ => 2,
        }
    }
    // `std::cmp::Reverse` makes min_by_key pick the candidate with the MOST
    // lines within a tier (largest line count = smallest Reverse value).
    // Previously used `usize::MAX - len`, which is arithmetically equivalent
    // but generates a surviving mutation for the `-` operator (both `-` and
    // `/` are monotonic w.r.t. len, so the mutation is observationally
    // equivalent for the tiebreaker ordering). Reverse carries no arithmetic
    // operator and is mutation-transparent.
    candidates
        .iter()
        .filter(|c| !c.lines.is_empty())
        .min_by_key(|c| (rank(&c.source), std::cmp::Reverse(c.lines.len())))
}

pub async fn run<C: MergeChat + ?Sized>(
    aai: &AaiBackend,
    chat: &C,
    audio_path: &Path,
    candidates: &[CandidateText],
    language: Option<&str>,
) -> Result<AsrResult, AsrError> {
    // 1) Transcribe with AAI.
    let transcript: AaiTranscript = match aai.transcribe(audio_path).await {
        Ok(t) => t,
        Err(AaiError::QuotaExhausted) => return Err(AsrError::QuotaExhausted),
        Err(e) => return Err(AsrError::Aai(e)),
    };
    let aai_word_count = transcript.words.len();
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
                claude_disagreement: None,
                claude_notes: None,
                claude_line_count: None,
                fallback_reason: None,
            },
        });
    }

    // 2) Pick untimed candidate (genius > lrclib > others).
    let cand = match pick_untimed_candidate(candidates) {
        Some(c) => c,
        None => return Err(AsrError::NoCandidate),
    };
    let untimed_text = cand.lines.join("\n");
    let input = ClaudeMergeInput {
        aai_words: &transcript.words,
        untimed_text: &untimed_text,
        untimed_source: &cand.source,
        language,
    };

    // 3) Claude-merge. Any failure → fallback path.
    let merged = match merge(chat, &input).await {
        Ok(m) => m,
        Err(MergeError::Transport(e)) => {
            tracing::warn!("asr_path: claude transport failed: {e} — falling back");
            return Ok(fallback_result(
                &transcript,
                aai_word_count,
                "transport",
                None,
                None,
                None,
            ));
        }
        Err(e) => {
            tracing::warn!("asr_path: claude merge rejected: {e} — falling back");
            return Ok(fallback_result(
                &transcript,
                aai_word_count,
                "rejected",
                None,
                None,
                None,
            ));
        }
    };

    if merged.disagreement || merged.lines.is_empty() {
        tracing::info!(
            disagreement = merged.disagreement,
            notes = %merged.notes,
            "asr_path: claude declined merge — using fallback"
        );
        return Ok(fallback_result(
            &transcript,
            aai_word_count,
            "disagreement_or_empty",
            Some(merged.disagreement),
            Some(merged.notes.clone()),
            Some(merged.lines.len()),
        ));
    }

    // 4) Resolve indices to ms.
    match resolve(&merged, &transcript) {
        Ok(lines) => Ok(AsrResult {
            output: AsrOutput::Merged {
                lines,
                source: SOURCE_MERGED,
            },
            audit: AsrAudit {
                outcome: "merged",
                source_label: Some(SOURCE_MERGED),
                quarantine_reason: None,
                aai_word_count,
                claude_disagreement: Some(false),
                claude_notes: Some(merged.notes.clone()),
                claude_line_count: Some(merged.lines.len()),
                fallback_reason: None,
            },
        }),
        Err(ResolverError::Empty) => Ok(fallback_result(
            &transcript,
            aai_word_count,
            "resolver_error",
            Some(merged.disagreement),
            Some(merged.notes.clone()),
            Some(merged.lines.len()),
        )),
        Err(e) => {
            tracing::warn!("asr_path: resolver rejected claude output: {e} — falling back");
            Ok(fallback_result(
                &transcript,
                aai_word_count,
                "resolver_error",
                Some(merged.disagreement),
                Some(merged.notes.clone()),
                Some(merged.lines.len()),
            ))
        }
    }
}

fn fallback_result(
    transcript: &AaiTranscript,
    aai_word_count: usize,
    fallback_reason: &'static str,
    claude_disagreement: Option<bool>,
    claude_notes: Option<String>,
    claude_line_count: Option<usize>,
) -> AsrResult {
    let output = fallback_output(transcript);
    let (outcome, source_label, quarantine_reason) = match &output {
        AsrOutput::Quarantine { reason } => ("quarantine", None, Some(*reason)),
        AsrOutput::Fallback { .. } => ("fallback", Some(SOURCE_FALLBACK), None),
        AsrOutput::Merged { .. } => unreachable!("fallback_output never returns Merged"),
    };
    AsrResult {
        output,
        audit: AsrAudit {
            outcome,
            source_label,
            quarantine_reason,
            aai_word_count,
            claude_disagreement,
            claude_notes,
            claude_line_count,
            fallback_reason: Some(fallback_reason),
        },
    }
}

fn fallback_output(transcript: &AaiTranscript) -> AsrOutput {
    let lines = fallback::split_on_silence(&transcript.words);
    if lines.is_empty() {
        AsrOutput::Quarantine {
            reason: "empty_fallback",
        }
    } else {
        AsrOutput::Fallback {
            lines,
            source: SOURCE_FALLBACK,
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
