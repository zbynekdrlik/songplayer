//! Tier-1 candidate type — the shared text-candidate carrier.
//!
//! The `pick_best` / `collect` WhisperX tier chooser and the `Tier1Result` /
//! `AlignedLines` / `FetchFn` machinery were deleted in #159 (one-regime
//! cleanup). Only `CandidateText` + `TIER1_MIN_LINES` survive: `CandidateText`
//! is the candidate the v21 reference stage force-aligns
//! (`claude_merge::best_authoritative_candidate` picks it), and
//! `TIER1_MIN_LINES` is the "full lyric sheet" threshold `spotify_resolver`
//! verifies against.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateText {
    /// "tier1:spotify" / "tier1:lrclib" / "tier1:yt_subs" / "genius" etc.
    pub source: String,
    pub lines: Vec<String>,
    /// `Some` when the fetcher has line-level timing. `start_ms`, `end_ms` per line.
    pub line_timings: Option<Vec<(u64, u64)>>,
    pub has_timing: bool,
}

// Bridge from the gather-side `provider::CandidateText`. Reverse direction
// lives in `provider.rs` next to its struct.
impl From<crate::lyrics::provider::CandidateText> for CandidateText {
    fn from(c: crate::lyrics::provider::CandidateText) -> Self {
        Self {
            source: c.source,
            lines: c.lines,
            line_timings: c.line_timings,
            has_timing: c.has_timing,
        }
    }
}

/// Minimum lines for a source to count as a full lyric sheet (not an intro
/// snippet / partial fetch). Used by `spotify_resolver`'s verification gate.
pub const TIER1_MIN_LINES: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_min_lines_is_ten() {
        assert_eq!(TIER1_MIN_LINES, 10);
    }

    #[test]
    fn from_provider_candidate_preserves_fields() {
        let p = crate::lyrics::provider::CandidateText {
            source: "lrclib".into(),
            lines: vec!["a".into(), "b".into()],
            line_timings: Some(vec![(0, 1000), (1000, 2000)]),
            has_timing: true,
        };
        let c = CandidateText::from(p);
        assert_eq!(c.source, "lrclib");
        assert_eq!(c.lines.len(), 2);
        assert!(c.has_timing);
        assert_eq!(c.line_timings.unwrap().len(), 2);
    }
}
