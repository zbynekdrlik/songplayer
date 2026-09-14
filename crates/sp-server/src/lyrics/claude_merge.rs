//! Source-priority + best-authoritative candidate selector for the lyrics
//! pipeline. `best_authoritative_candidate` picks the text candidate the v21
//! (#143) mtl reference stage force-aligns.
//!
//! Per `feedback_line_timing_only.md`: every output line ships `words: None`.

use crate::lyrics::tier1::CandidateText;

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
/// Returns a reference to the chosen `CandidateText` so the v21 reference
/// stage can read both `lines` (the text to force-align via mtl) and `source`
/// (for the persisted `<source>+mtl@rev1/g35t-ok` label). Returns `None` for
/// empty input.
pub(crate) fn best_authoritative_candidate(candidates: &[CandidateText]) -> Option<&CandidateText> {
    candidates
        .iter()
        .max_by_key(|c| (priority_with_timing(&c.source, c.has_timing), c.lines.len()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "claude_merge_tests.rs"]
mod tests;
