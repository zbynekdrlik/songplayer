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
    // 1 ref line "holy holy holy". ASR sings it twice with a > 4s gap between
    // (the chorus repeat).
    let ref_lines = vec!["holy holy holy".to_string()];
    let asr_track = asr(vec![
        make_word("holy", 0, 500),
        make_word("holy", 600, 1100),
        make_word("holy", 1200, 1700),
        // long instrumental pause modelled by gap in indexing
        make_word("holy", 6000, 6500),
        make_word("holy", 6600, 7100),
        make_word("holy", 7200, 7700),
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
    // Re-emit should reference the second-half indices (3..=5).
    let min = *emit.asr_word_indices.iter().min().unwrap();
    assert!(min >= 3, "chorus re-emit must point at the unmatched gap");
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
    assert_eq!(lines[0].start_ms, 1000);
    assert_eq!(lines[0].end_ms, 1000 + LONG_LINE_CAP_MS);
}

#[test]
fn apply_cap_and_monotonic_floor_clamps_overlap() {
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
    assert!(lines[1].start_ms >= lines[0].end_ms);
    assert!(lines[1].end_ms > lines[1].start_ms);
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
