//! Timed-reference merge pipeline (2026-05-07 unification).
//!
//! Routes timed sources (`tier1:spotify`, line-synced `lrclib` /
//! `tier1:lrclib`, `tier1:yt_subs` with timing) when their line timings
//! cover at least 80% of the song duration.
//!
//! Two modes:
//!
//! - **Mode A** (`asr = Some(...)`): timed-TextOnly route. Emit reference
//!   text + reference timings (the timed source IS the source of truth;
//!   WhisperX provided word timings only for downstream features that
//!   are not yet relevant here). Apply line-split for the 32-char cap.
//! - **Mode B** (`asr = None`): LineSynced short-circuit. Same emit +
//!   line-split.
//!
//! Provenance: `{candidate.source}+timed-merge`. Output lines always have
//! `words: None` per `feedback_line_timing_only.md`.
//!
//! Future-work follow-up (out of scope for this PR): apply the same
//! phantom-cluster filter (sustained-note absorb) and the same advanced
//! sanitize pass that `text_reference_merge` runs. Those helpers currently
//! live as private helpers inside `text_reference_merge_phantom.rs` and
//! `text_reference_merge_*_mapping.rs`; extracting them as `pub(crate)`
//! is its own refactor and lands in a follow-up PR. The current PR
//! preserves parity with the pre-fix `claude_merge::merge + split_track`
//! baseline (32-char cap only), so timed-source output does not regress.

use thiserror::Error;
use tracing::info;

use crate::ai::client::AiClient;
use crate::lyrics::audit_ctx::AuditContext;
use crate::lyrics::backend::{AlignedLine, AlignedTrack};
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::tier1::CandidateText;

#[derive(Debug, Error)]
pub enum TimedMergeError {
    #[error("candidate has no line timings")]
    NoTimings,
    #[error("candidate has zero reference lines")]
    EmptyReference,
    #[error("candidate has timings/lines length mismatch (lines={lines}, timings={timings})")]
    LengthMismatch { lines: usize, timings: usize },
}

/// Public entry: timed-merge for both LineSynced (asr=None) and
/// timed-TextOnly (asr=Some) routes. yt_subs is NOT routed through
/// here — the orchestrator clusters its caption-window-broken lines
/// and dispatches to text_reference_merge directly.
pub async fn process(
    _ai_client: Option<&AiClient>,
    asr: Option<&AlignedTrack>,
    candidate: &CandidateText,
    song_duration_ms: u32,
    _audit: Option<&AuditContext<'_>>,
) -> Result<AlignedTrack, TimedMergeError> {
    if candidate.lines.is_empty() {
        return Err(TimedMergeError::EmptyReference);
    }
    let timings = match &candidate.line_timings {
        Some(t) => t,
        None => return Err(TimedMergeError::NoTimings),
    };
    if timings.len() != candidate.lines.len() {
        return Err(TimedMergeError::LengthMismatch {
            lines: candidate.lines.len(),
            timings: timings.len(),
        });
    }

    let aligned_lines = candidate_to_aligned_lines(candidate);

    info!(
        source = %candidate.source,
        lines = aligned_lines.len(),
        song_duration_ms,
        mode = if asr.is_some() { "A" } else { "B" },
        "timed_reference_merge: emit reference timed lines"
    );

    let pre_split = AlignedTrack {
        lines: aligned_lines,
        provenance: format!("{}+timed-merge", candidate.source),
        raw_confidence: asr.map(|a| a.raw_confidence).unwrap_or(1.0),
    };
    Ok(split_track(&pre_split, SplitConfig::default()))
}

/// Helper: convert a timed `CandidateText` to a `Vec<AlignedLine>`. Caller
/// must have verified `line_timings` is `Some` and matches `lines.len()`.
pub(crate) fn candidate_to_aligned_lines(candidate: &CandidateText) -> Vec<AlignedLine> {
    let timings = candidate
        .line_timings
        .as_ref()
        .expect("caller-verified Some");
    candidate
        .lines
        .iter()
        .zip(timings.iter())
        .map(|(text, (start, end))| AlignedLine {
            text: text.clone(),
            start_ms: (*start) as u32,
            end_ms: (*end) as u32,
            words: None,
        })
        .collect()
}

#[cfg(test)]
#[path = "timed_reference_merge_tests.rs"]
mod tests;
