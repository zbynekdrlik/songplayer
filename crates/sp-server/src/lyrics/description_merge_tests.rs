//! Tests for description_merge phases 1, 2, 4, 5 (Phase 3 Claude path needs
//! a mock AiClient and is exercised end-to-end on win-resolume reprocess
//! verification, not in unit tests). Sibling-included from description_merge.rs.

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

fn asr(words: Vec<AlignedWord>) -> AlignedTrack {
    AlignedTrack {
        lines: vec![AlignedLine {
            text: "(combined)".into(),
            start_ms: 0,
            end_ms: 60000,
            words: Some(words),
        }],
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    }
}

// ── Phase 1: match_ref_to_asr ─────────────────────────────────────────────────

#[test]
fn match_ref_to_asr_assigns_words_to_matching_lines() {
    let ref_lines = vec![
        "holy is the lord".to_string(),
        "worthy is the king".to_string(),
    ];
    let asr_track = asr(vec![
        make_word("holy", 0, 500),
        make_word("is", 600, 800),
        make_word("the", 900, 1100),
        make_word("lord", 1200, 1700),
        make_word("worthy", 3000, 3600),
        make_word("is", 3700, 3900),
        make_word("the", 4000, 4200),
        make_word("king", 4300, 4900),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 2);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2, 3]);
    assert_eq!(emits[1].asr_word_indices, vec![4, 5, 6, 7]);
}

// ── Phase 2: chorus repeat detection ──────────────────────────────────────────

#[test]
fn detect_chorus_repeats_emits_for_long_unmatched_gap() {
    // 1 ref line "holy holy holy". ASR sings it twice. Both chorus
    // occurrences span > 4 s so whichever side LCS consumes, the other
    // exceeds CHORUS_REPEAT_GAP_MS (4000) and triggers the re-emit.
    // (LCS backtrack is greedy-from-end → Phase 1 consumes the second
    // chorus; Phase 2 re-emits the first.)
    let ref_lines = vec!["holy holy holy".to_string()];
    let asr_track = asr(vec![
        make_word("holy", 0, 500),
        make_word("holy", 2500, 3000),
        make_word("holy", 4500, 5000),
        // long instrumental pause; second-pass chorus repeats:
        make_word("holy", 9000, 9500),
        make_word("holy", 11500, 12000),
        make_word("holy", 13500, 14000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    let extras = detect_chorus_repeats(&ref_lines, &asr_words, &emits);
    assert!(
        !extras.is_empty(),
        "expected at least one chorus repeat re-emit; got {:?}",
        extras
    );
    let emit = &extras[0];
    assert_eq!(emit.text, "holy holy holy");
    // LCS backtrack is greedy-from-end so Phase 1 actually consumed the LAST
    // three indices and Phase 2 re-emits at the FIRST three (the unmatched
    // window from index 0..2). Either side is valid for chorus-repeat
    // semantics — the assertion is just that emit indices are disjoint from
    // Phase 1's consumed set.
    let phase1_consumed: std::collections::HashSet<usize> = emits
        .iter()
        .flat_map(|e| e.asr_word_indices.iter().copied())
        .collect();
    let phase2_consumed: std::collections::HashSet<usize> =
        emit.asr_word_indices.iter().copied().collect();
    assert!(
        phase1_consumed.is_disjoint(&phase2_consumed),
        "chorus re-emit must point at audio words NOT consumed in Phase 1; phase1={:?} phase2={:?}",
        phase1_consumed,
        phase2_consumed
    );
}

// ── Phase 2.5: trim_outlier_indices ───────────────────────────────────────────

#[test]
fn trim_outlier_indices_keeps_tight_match_intact() {
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 400),
        make_word("c", 500, 700),
        make_word("d", 800, 1000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1, 2, 3];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0, 1, 2, 3]);
}

#[test]
fn trim_outlier_indices_drops_trailing_outlier_past_cap() {
    // 5 words: [0..4 contiguous within 7.5s] + [5 jumped to 20s].
    // Span 20s > LONG_LINE_CAP_MS=8s, drop trailing.
    let asr_track = asr(vec![
        make_word("a", 0, 500),
        make_word("b", 1000, 1500),
        make_word("c", 3000, 3500),
        make_word("d", 5000, 5500),
        make_word("e", 7000, 7500),
        make_word("outlier", 19000, 20000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1, 2, 3, 4, 5];
    trim_outlier_indices(&mut indices, &asr_words);
    // After trim: [0..4] span 7.5s within cap.
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);
}

#[test]
fn trim_outlier_indices_drops_to_single_when_two_entry_span_exceeds_cap() {
    // 2-entry emit with span > cap — trim drops the trailing outlier so
    // only [0] remains. Phase 5 will then reject the single-word residual
    // via its MIN_LINE_DURATION_MS micro-window drop. Reproduces id=132
    // 2026-05-04 case where Phase 2 LCS for "Holy forever" picked the
    // first "holy" + a far-later "forever" across multiple sung phrases.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 50000, 50100), // 50s gap — exceeds cap
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_keeps_single_entry_intact() {
    // Single-entry input is untouched (no second word to compare span).
    let asr_track = asr(vec![make_word("a", 1000, 1500)]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_handles_unsorted_input() {
    // Indices arrive ascending after Phase 1 sort, but defensive: trim should
    // sort before measuring span.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 400),
        make_word("c", 500, 700),
        make_word("outlier", 20000, 21000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![3, 0, 1, 2];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0, 1, 2]);
}

// ── Phase 4: aligned_lines_for_emit ───────────────────────────────────────────

#[test]
fn aligned_lines_for_emit_single_uses_min_max_word_timing() {
    let asr_track = asr(vec![
        make_word("a", 100, 300),
        make_word("b", 400, 600),
        make_word("c", 700, 900),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "a b c".into(),
        asr_word_indices: vec![0, 1, 2],
    };
    let lines = aligned_lines_for_emit(&emit, &asr_words, None);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_ms, 100);
    assert_eq!(lines[0].end_ms, 900);
    assert!(lines[0].words.is_none());
    assert_eq!(lines[0].text, "a b c");
}

#[test]
fn aligned_lines_for_emit_with_subs_assigns_per_sub_word_timing() {
    // Parent text "alpha bravo charlie delta". Subs split "alpha bravo" /
    // "charlie delta". Each sub gets timing from its constituent ASR words.
    let asr_track = asr(vec![
        make_word("alpha", 100, 300),
        make_word("bravo", 400, 700),
        make_word("charlie", 1500, 2000),
        make_word("delta", 2100, 2500),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "alpha bravo charlie delta".into(),
        asr_word_indices: vec![0, 1, 2, 3],
    };
    let subs = vec!["alpha bravo".to_string(), "charlie delta".to_string()];
    let lines = aligned_lines_for_emit(&emit, &asr_words, Some(&subs));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "alpha bravo");
    assert_eq!(lines[0].start_ms, 100);
    assert_eq!(lines[0].end_ms, 700);
    assert_eq!(lines[1].text, "charlie delta");
    assert_eq!(lines[1].start_ms, 1500);
    assert_eq!(lines[1].end_ms, 2500);
}

// ── Phase 5: cap + monotonic ──────────────────────────────────────────────────

#[test]
fn apply_cap_and_monotonic_caps_long_line_to_8s() {
    let mut lines = vec![AlignedLine {
        text: "long".into(),
        start_ms: 1000,
        end_ms: 60000, // 59 s — way over 8 s cap
        words: None,
    }];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_ms, 1000);
    assert_eq!(lines[0].end_ms, 1000 + LONG_LINE_CAP_MS);
}

#[test]
fn apply_cap_and_monotonic_floor_clamps_overlap() {
    // Both lines have original dur 1000ms (>= MIN). After floor-clamp the
    // second's start_ms is pushed to 1000 (the first's end_ms), leaving its
    // dur at 500ms — equal to MIN_LINE_DURATION_MS so it's kept.
    let mut lines = vec![
        AlignedLine {
            text: "a".into(),
            start_ms: 0,
            end_ms: 1000,
            words: None,
        },
        AlignedLine {
            text: "b".into(),
            start_ms: 500,
            end_ms: 1500,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert!(lines[1].start_ms >= lines[0].end_ms);
    assert!(lines[1].end_ms > lines[1].start_ms);
}

#[test]
fn apply_cap_and_monotonic_drops_pre_clamp_micro_window() {
    // Original duration 200ms < 500ms threshold — dropped, no flash.
    let mut lines = vec![
        AlignedLine {
            text: "real".into(),
            start_ms: 0,
            end_ms: 1500,
            words: None,
        },
        AlignedLine {
            text: "flash".into(),
            start_ms: 2000,
            end_ms: 2200, // 200ms — micro-window
            words: None,
        },
        AlignedLine {
            text: "more".into(),
            start_ms: 3000,
            end_ms: 5000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "real");
    assert_eq!(lines[1].text, "more");
}

#[test]
fn apply_cap_and_monotonic_extends_end_to_next_start() {
    // Two lines with a short silence between them. Line A originally ends
    // at 2000ms; Line B starts at 3500ms. Wall would flicker blank for
    // 1500ms without this pass. Extension pulls A.end forward to B.start.
    let mut lines = vec![
        AlignedLine {
            text: "A".into(),
            start_ms: 0,
            end_ms: 2000,
            words: None,
        },
        AlignedLine {
            text: "B".into(),
            start_ms: 3500,
            end_ms: 5000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].end_ms, 3500, "A should extend to B.start");
    assert_eq!(lines[1].start_ms, 3500);
}

#[test]
fn apply_cap_and_monotonic_extension_uncapped_no_gap() {
    // No gap, EVER. Operator directive 2026-05-04. Even a 29 s instrumental
    // gap between sung phrases: the line stays visible across the whole
    // silence until the next phrase's first sung word begins.
    let mut lines = vec![
        AlignedLine {
            text: "A".into(),
            start_ms: 0,
            end_ms: 1000,
            words: None,
        },
        AlignedLine {
            text: "B".into(),
            start_ms: 30_000,
            end_ms: 32_000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].end_ms, 30_000, "A extends to B.start, no cap");
    assert_eq!(lines[1].start_ms, 30_000);
}

#[test]
fn apply_cap_and_monotonic_last_line_no_extension() {
    let mut lines = vec![AlignedLine {
        text: "only".into(),
        start_ms: 0,
        end_ms: 1500,
        words: None,
    }];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].end_ms, 1500, "last line keeps original end_ms");
}

#[test]
fn apply_cap_and_monotonic_drops_post_clamp_collapse() {
    // Both lines start near 1000ms, both 600ms duration. After floor-clamp
    // the second's window collapses to <500ms — drop it.
    let mut lines = vec![
        AlignedLine {
            text: "a".into(),
            start_ms: 0,
            end_ms: 1100,
            words: None,
        },
        AlignedLine {
            text: "collapses".into(),
            start_ms: 700,
            end_ms: 1300, // post-clamp would be 1100..1300 = 200ms
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "a");
}

// ── deterministic_split_one ───────────────────────────────────────────────────

#[test]
fn deterministic_split_one_short_line_kept_intact() {
    let s = deterministic_split_one("Holy forever");
    assert_eq!(s, vec!["Holy forever".to_string()]);
}

#[test]
fn deterministic_split_one_long_line_splits_at_word_boundary_under_cap() {
    let s = deterministic_split_one("A thousand generations falling down in worship");
    // Each sub must be <= 32 chars.
    for sub in &s {
        assert!(
            sub.chars().count() <= SUBLINE_MAX_CHARS,
            "sub over cap: {:?}",
            sub
        );
    }
    // Joined back (with single space) must equal original (modulo trim).
    let joined = s.join(" ");
    assert_eq!(joined, "A thousand generations falling down in worship");
}

#[test]
fn deterministic_split_one_long_line_with_comma_prefers_comma_break() {
    let s = deterministic_split_one("And the angels cry, holy forever amen");
    assert!(s.len() >= 2);
    // First sub ends at comma — last char of first piece is ','.
    assert!(
        s[0].ends_with(','),
        "expected comma at end of first sub: {:?}",
        s
    );
}

// ── parse_split_response ──────────────────────────────────────────────────────

#[test]
fn parse_split_response_extracts_clean_json() {
    let raw = r#"{"splits":[{"i":0,"subs":[{"en":"alpha"},{"en":"beta"}]}]}"#;
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 0);
    assert_eq!(parsed.splits[0].subs.len(), 2);
    assert_eq!(parsed.splits[0].subs[0].en, "alpha");
}

#[test]
fn parse_split_response_strips_prose_preamble() {
    let raw = "Here you go:\n```json\n{\"splits\":[{\"i\":7,\"subs\":[{\"en\":\"x\"}]}]}\n```";
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 7);
}
