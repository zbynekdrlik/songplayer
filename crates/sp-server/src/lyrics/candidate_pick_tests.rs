//! Tests for `candidate_pick`. Included as a sibling file via
//! `#[path = "candidate_pick_tests.rs"] #[cfg(test)] mod tests;` from candidate_pick.rs
//! to keep that file under the airuleset 1000-line cap.

use super::*;

// ── priority_with_timing tests ───────────────────────────────────────────

#[cfg(test)]
mod priority_with_timing_tests {
    use super::*;

    /// The full source × timing priority table.
    /// Tier-break: timed sources outrank text-only of the same name; description outranks
    /// every other text source.
    #[test]
    fn matrix_matches_spec_table() {
        // (source, has_timing, expected_priority)
        let cases: &[(&str, bool, u32)] = &[
            ("override", false, 6),
            ("tier1:spotify", true, 5),
            ("lrclib", true, 5),
            ("tier1:lrclib", true, 5),
            ("tier1:yt_subs", true, 4),
            ("yt_subs", true, 4),
            ("description", false, 3),
            ("lrclib", false, 2),
            ("tier1:lrclib", false, 2),
            ("lyrics_ovh", false, 1),
            // #232: Genius is gone; its old label ranks nowhere.
            ("genius", false, 0),
            ("yt_subs", false, 0),
            ("tier1:yt_subs", false, 0),
            ("unknown_source", false, 0),
        ];
        for (source, has_timing, expected) in cases {
            assert_eq!(
                priority_with_timing(source, *has_timing),
                *expected,
                "priority_with_timing({source:?}, {has_timing}) expected {expected}",
            );
        }
    }
}

#[cfg(test)]
mod best_authoritative_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn text_cand(source: &str, line_count: usize) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: None,
            has_timing: false,
        }
    }

    fn timed_cand(source: &str, line_count: usize, span_ms: u64) -> CandidateText {
        let timings: Vec<(u64, u64)> = (0..line_count as u64)
            .map(|i| {
                let start = i * (span_ms / line_count.max(1) as u64);
                let end = start + 1000;
                (start, end)
            })
            .collect();
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    /// id=21 "Good Shepherd" regression: description (26 lines) + a community
    /// lyrics text (70 lines, then Genius, now lyrics.ovh at the same slot) both
    /// present. Pre-fix: the community text won (priority 2 > description 0).
    /// Post-fix: description wins (priority 3 > lyrics_ovh 1) by spec.
    #[test]
    fn description_beats_lyrics_ovh_when_both_present() {
        let candidates = vec![text_cand("description", 26), text_cand("lyrics_ovh", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "description");
    }

    #[test]
    fn override_beats_description() {
        let candidates = vec![text_cand("description", 26), text_cand("override", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "override");
    }

    #[test]
    fn timed_lrclib_beats_text_description() {
        let candidates = vec![
            text_cand("description", 26),
            timed_cand("lrclib", 30, 50_000),
        ];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
        assert!(best.has_timing);
    }

    #[test]
    fn lrclib_text_beats_lyrics_ovh_text() {
        let candidates = vec![text_cand("lyrics_ovh", 70), text_cand("lrclib", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
    }

    #[test]
    fn empty_candidates_returns_none() {
        let candidates: Vec<CandidateText> = vec![];
        assert!(best_authoritative_candidate(&candidates).is_none());
    }

    #[test]
    fn tie_break_prefers_longer_lines_at_same_priority() {
        let candidates = vec![text_cand("lyrics_ovh", 30), text_cand("lyrics_ovh", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.lines.len(), 70);
    }
}
