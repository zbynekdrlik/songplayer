//! Source-priority + best-authoritative selector + coverage helper for the
//! lyrics-merge pipeline. Used by the orchestrator to choose between
//! `text_reference_merge` (text-only references) and `timed_reference_merge`
//! (line-timed references — added in Phase D).
//!
//! Per `feedback_line_timing_only.md`: every output line ships `words: None`.
//! Per `feedback_no_even_distribution.md`: timing comes from WhisperX words only.

use thiserror::Error;

use crate::lyrics::backend::AlignedWord;
use crate::lyrics::tier1::CandidateText;

// ── Errors ────────────────────────────────────────────────────────────────────
//
// This enum is shared with `text_reference_merge` (description / override path).
// Both branches return the same error type up to `Orchestrator::process`.

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("no usable text candidate")]
    NoReference,
    #[error("Claude call failed: {0}")]
    Claude(#[from] anyhow::Error),
    #[error("parse failed: {0}")]
    ParseFailed(String),
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Priority for the `best_authoritative_candidate` selector.
///
/// Spec: docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md
/// — text-canonical (description) outranks every other text-only source;
/// timed sources outrank text-only of the same name; override is highest.
///
/// `has_timing` matters because the same source label (`lrclib`, `yt_subs`,
/// `tier1:lrclib`, `tier1:yt_subs`) can be either timed or text-only depending
/// on the candidate.
pub(crate) fn priority_with_timing(source: &str, has_timing: bool) -> u32 {
    if source == "override" {
        return 6;
    }
    if has_timing {
        if source.starts_with("tier1:spotify") {
            return 5;
        }
        if source == "lrclib" || source.starts_with("tier1:lrclib") {
            return 5;
        }
        if source == "yt_subs" || source.starts_with("tier1:yt_subs") {
            return 4;
        }
        return 0;
    }
    if source == "description" {
        return 3;
    }
    if source == "lrclib" || source.starts_with("tier1:lrclib") {
        return 2;
    }
    if source == "genius" || source.starts_with("tier1:genius") {
        return 1;
    }
    // text yt_subs / tier1:yt_subs / unknown sources all fall through
    // to priority 0 — explicit yt_subs branch is removed as equivalent.
    0
}

/// Pick the strongest authoritative candidate by `priority_with_timing`,
/// breaking ties by line count (longest wins).
///
/// Returns a reference to the chosen `CandidateText` so callers can read
/// both `lines` (for merging) and `source` (for choosing the merge path —
/// timed routes through `timed_reference_merge::process`, text-only routes
/// through `text_reference_merge::process`). Returns `None` for empty input.
pub(crate) fn best_authoritative_candidate(candidates: &[CandidateText]) -> Option<&CandidateText> {
    candidates
        .iter()
        .max_by_key(|c| (priority_with_timing(&c.source, c.has_timing), c.lines.len()))
}

/// Coverage check for timed-reference routing.
///
/// Returns `true` when the candidate has line timings AND the span from the
/// first line's `start_ms` to the last line's `end_ms` is at least 80% of
/// `song_duration_ms`. The 80% floor protects against partial-fetch sources
/// (e.g. spotify returning only the first verse). Below the floor, the timed
/// routing layer falls back to text-merge.
///
/// Returns `false` when:
/// - `line_timings` is None or empty
/// - `song_duration_ms` is 0
/// - the timing span covers less than 80% of `song_duration_ms`
pub(crate) fn coverage_ok(candidate: &CandidateText, song_duration_ms: u32) -> bool {
    if song_duration_ms == 0 {
        return false;
    }
    let timings = match &candidate.line_timings {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let first_start = timings.first().map(|(s, _)| *s).unwrap_or(0);
    let last_end = timings.last().map(|(_, e)| *e).unwrap_or(0);
    let span = last_end.saturating_sub(first_start);
    let threshold = (song_duration_ms as u64) * 80 / 100;
    span >= threshold
}

/// Drop WhisperX hallucinated lead-in words.
///
/// While the first word's duration > 1500ms AND the gap between word[0].end_ms
/// and word[1].start_ms > 2000ms, drop word[0].
///
/// Returns the trimmed word list (may be empty if all words were dropped, though
/// that can only happen for a 1-word list where the gap check can't apply).
pub(super) fn drop_hallucinated_lead_in(mut words: Vec<AlignedWord>) -> Vec<AlignedWord> {
    loop {
        if words.len() < 2 {
            break;
        }
        let duration = words[0].end_ms.saturating_sub(words[0].start_ms);
        let gap = words[1].start_ms.saturating_sub(words[0].end_ms);
        if duration > 1500 && gap > 2000 {
            words.remove(0);
        } else {
            break;
        }
    }
    words
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "claude_merge_tests.rs"]
mod tests;
