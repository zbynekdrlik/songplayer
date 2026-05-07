//! Tests for `claude_merge`. Included as a sibling file via
//! `#[path = "claude_merge_tests.rs"] #[cfg(test)] mod tests;` from claude_merge.rs
//! to keep that file under the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::tier1::CandidateText;

fn make_word(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.to_string(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

// ── drop_hallucinated_lead_in tests ──────────────────────────────────────

#[test]
fn drop_lead_in_removes_long_duration_word_with_large_gap() {
    // Word 0: duration = 2000ms (> 1500), gap to word 1 = 3000ms (> 2000) → drop
    let words = vec![
        make_word("ohhh", 0, 2000),
        make_word("alleluia", 5000, 6000),
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].text, "alleluia");
}

#[test]
fn drop_lead_in_keeps_word_when_duration_under_threshold() {
    // Word 0: duration = 1000ms (≤ 1500) → keep even with large gap
    let words = vec![
        make_word("yeah", 0, 1000),
        make_word("alleluia", 5000, 6000),
    ];
    let result = drop_hallucinated_lead_in(words.clone());
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].text, "yeah");
}

#[test]
fn drop_lead_in_keeps_word_when_gap_under_threshold() {
    // Word 0: duration = 2000ms (> 1500), but gap = 1000ms (≤ 2000) → keep
    let words = vec![
        make_word("ohhh", 0, 2000),
        make_word("alleluia", 3000, 4000),
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].text, "ohhh");
}

#[test]
fn drop_lead_in_handles_single_word() {
    let words = vec![make_word("alone", 0, 5000)];
    let result = drop_hallucinated_lead_in(words.clone());
    assert_eq!(result.len(), 1);
}

// ── drop_hallucinated_lead_in: exact boundary tests (line 230 mutants) ──
//
// Mutant A: `duration > 1500` → `duration >= 1500` would drop a word with
// duration == 1500ms, but the correct code keeps it.
// Mutant B: `gap > 2000` → `gap >= 2000` would drop a word with gap == 2000ms.

#[test]
fn drop_lead_in_keeps_word_at_exactly_1500ms_duration() {
    // duration == 1500 (NOT > 1500 → should NOT drop)
    let words = vec![
        make_word("hmm", 0, 1500), // duration = 1500 exactly — boundary, must keep
        make_word("alleluia", 4000, 5000), // gap = 2500 (> 2000)
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(
        result.len(),
        2,
        "duration == 1500 must NOT be dropped (threshold is > 1500)"
    );
    assert_eq!(result[0].text, "hmm");
}

#[test]
fn drop_lead_in_keeps_word_at_exactly_2000ms_gap() {
    // gap == 2000 (NOT > 2000 → should NOT drop)
    let words = vec![
        make_word("ohhh", 0, 2000),        // duration = 2000 > 1500 ✓
        make_word("alleluia", 4000, 5000), // gap = 4000 - 2000 = 2000 exactly — boundary, must keep
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(
        result.len(),
        2,
        "gap == 2000 must NOT be dropped (threshold is > 2000)"
    );
    assert_eq!(result[0].text, "ohhh");
}

// ── priority_with_timing tests ───────────────────────────────────────────

#[cfg(test)]
mod priority_with_timing_tests {
    use super::*;

    /// Spec table from docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md.
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
            ("genius", false, 1),
            ("tier1:genius", false, 1),
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
mod coverage_ok_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn cand_with_timings(timings: Vec<(u64, u64)>) -> CandidateText {
        CandidateText {
            source: "lrclib".into(),
            lines: vec!["x".into(); timings.len()],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    #[test]
    fn returns_true_when_span_covers_at_least_80_percent_of_duration() {
        // 0..240000 ms span, 300000 ms duration → 80% exact → true
        let c = cand_with_timings(vec![(0, 1000), (239000, 240000)]);
        assert!(coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_span_below_80_percent() {
        // 0..200000 ms span, 300000 ms duration → 66.7% → false
        let c = cand_with_timings(vec![(0, 1000), (199000, 200000)]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_no_timings() {
        let c = CandidateText {
            source: "genius".into(),
            lines: vec!["x".into()],
            line_timings: None,
            has_timing: false,
        };
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_empty_timings() {
        let c = cand_with_timings(vec![]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_duration_zero() {
        let c = cand_with_timings(vec![(0, 1000)]);
        assert!(!coverage_ok(&c, 0));
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

    /// id=21 "Good Shepherd" regression: description (26 lines) + genius (70 lines)
    /// both present. Pre-fix: genius wins (priority 2 > description 0). Post-fix:
    /// description wins (priority 3 > genius 1) by spec.
    #[test]
    fn description_beats_genius_when_both_present() {
        let candidates = vec![text_cand("description", 26), text_cand("genius", 70)];
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
    fn timed_lrclib_beats_text_description_regardless_of_coverage() {
        // Selection layer ignores coverage — that's the routing layer's call.
        let candidates = vec![
            text_cand("description", 26),
            timed_cand("lrclib", 30, 50_000),
        ];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
        assert!(best.has_timing);
    }

    #[test]
    fn lrclib_text_beats_genius_text() {
        let candidates = vec![text_cand("genius", 70), text_cand("lrclib", 26)];
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
        let candidates = vec![text_cand("genius", 30), text_cand("genius", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.lines.len(), 70);
    }
}
