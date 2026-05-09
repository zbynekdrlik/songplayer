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
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::tier1::CandidateText;
use crate::lyrics::yt_subs_split::split_long_line_with_anchors;

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
/// timed-TextOnly (asr=Some) routes.
///
/// For yt_subs sources with ASR available, long lines (>32 chars) are
/// re-broken via `yt_subs_split::split_long_line_with_anchors` —
/// Claude picks karaoke-friendly phrase boundaries; whisperx provides
/// internal sub-line start_ms; yt_subs anchors are preserved at the
/// first sub's start and the last sub's end. Short lines and non-yt_subs
/// timed sources keep the legacy `split_track` path (32-char cap only).
pub async fn process(
    ai_client: Option<&AiClient>,
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

    // yt_subs path: re-break long lines via Claude + whisperx boundaries.
    let is_yt_subs = candidate.source == "yt_subs" || candidate.source.starts_with("tier1:yt_subs");
    if let (true, Some(ai), Some(asr_track)) = (is_yt_subs, ai_client, asr) {
        let asr_words: Vec<AlignedWord> = asr_track
            .lines
            .iter()
            .filter_map(|l| l.words.as_ref())
            .flatten()
            .cloned()
            .collect();
        let mut output: Vec<AlignedLine> = Vec::with_capacity(aligned_lines.len());
        for line in &aligned_lines {
            let split = split_long_line_with_anchors(
                ai,
                &line.text,
                line.start_ms,
                line.end_ms,
                &asr_words,
            )
            .await;
            output.extend(split);
        }
        return Ok(AlignedTrack {
            lines: output,
            provenance: format!("{}+timed-merge", candidate.source),
            raw_confidence: asr_track.raw_confidence,
        });
    }

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
