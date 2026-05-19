//! ASR alignment path — runs AssemblyAI U3-Pro + Claude-merge for songs
//! whose `gather_sources` returns ONLY untimed text candidates (genius,
//! lrclib-untimed, etc.). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`.

pub mod aai_backend;
pub mod claude_merge;
pub mod fallback;
pub mod merge_prompt;
pub mod resolver;

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

#[derive(Debug)]
pub enum AsrOutput {
    Merged {
        lines: Vec<LyricsLine>,
        source: &'static str,
    },
    Fallback {
        lines: Vec<LyricsLine>,
        source: &'static str,
    },
    Quarantine {
        reason: &'static str,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("AAI quota exhausted — surface to user, no row write")]
    QuotaExhausted,
    #[error("AAI transcription failed: {0}")]
    Aai(#[from] AaiError),
    #[error("Claude merge transport failed: {0}")]
    MergeTransport(String),
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
    candidates
        .iter()
        .filter(|c| !c.lines.is_empty())
        .min_by_key(|c| (rank(&c.source), usize::MAX - c.lines.len()))
}

pub async fn run<C: MergeChat + ?Sized>(
    aai: &AaiBackend,
    chat: &C,
    audio_path: &Path,
    candidates: &[CandidateText],
    language: Option<&str>,
) -> Result<AsrOutput, AsrError> {
    // 1) Transcribe with AAI.
    let transcript: AaiTranscript = match aai.transcribe(audio_path).await {
        Ok(t) => t,
        Err(AaiError::QuotaExhausted) => return Err(AsrError::QuotaExhausted),
        Err(e) => return Err(AsrError::Aai(e)),
    };
    if transcript.words.is_empty() {
        return Ok(AsrOutput::Quarantine {
            reason: "empty_transcript",
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
            return Ok(fallback_output(&transcript));
        }
        Err(e) => {
            tracing::warn!("asr_path: claude merge rejected: {e} — falling back");
            return Ok(fallback_output(&transcript));
        }
    };

    if merged.disagreement || merged.lines.is_empty() {
        tracing::info!(
            disagreement = merged.disagreement,
            notes = %merged.notes,
            "asr_path: claude declined merge — using fallback"
        );
        return Ok(fallback_output(&transcript));
    }

    // 4) Resolve indices to ms.
    match resolve(&merged, &transcript) {
        Ok(lines) => Ok(AsrOutput::Merged {
            lines,
            source: SOURCE_MERGED,
        }),
        Err(ResolverError::Empty) => Ok(fallback_output(&transcript)),
        Err(e) => {
            tracing::warn!("asr_path: resolver rejected claude output: {e} — falling back");
            Ok(fallback_output(&transcript))
        }
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
