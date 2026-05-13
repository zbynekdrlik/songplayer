//! Tests for text_reference_merge Phase 4 (aligned_lines_for_emit).
//!
//! Extracted from `text_reference_merge_tests.rs` (issue #90 fixup) to keep
//! both sibling test files under the 1000-line cap.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};

fn make_word(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.to_string(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn make_word_with_conf(text: &str, start_ms: u32, end_ms: u32, confidence: f32) -> AlignedWord {
    AlignedWord {
        text: text.to_string(),
        start_ms,
        end_ms,
        confidence,
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

// ── Phase 4: aligned_lines_for_emit ───────────────────────────────────────────

#[test]
fn emit_single_skips_near_zero_conf_artifact_start_word() {
    // id=227 "Thank You, Thank You" / "For the wonders You've done":
    // WhisperX fused "thank you for the wonders..." into one ASR line.
    // The forced-alignment placed:
    //   idx=0: "thank"  55960-56921ms conf=0.844
    //   idx=1: "you"    56981-57282ms  conf=0.732    ← first "Thank You"
    //   idx=2: "thank"  57302-57582ms  conf=0.174
    //   idx=3: "you"    57602-57662ms  conf=0.003    ← second "Thank You" (60ms, conf≈0)
    //   idx=4: "for"    57682-57742ms  conf=0.0      ← 60ms boundary artefact
    //   idx=5: "the"    60344-61045ms  conf=0.85     ← real start of "For the wonders"
    //   idx=6: "wonders" ...
    //
    // Test-data note: "the" + "wonders" confidence is raised to ≥ 0.75 so
    // phantom::drop_phantom_clusters does NOT fire on them. The production
    // descmerge_audit for id=227 lists these tokens as still present in the
    // post-flatten asr_words slice, so this matches the real scenario; the
    // raise keeps the unit test isolated from phantom-drop behaviour.
    //
    // Claude mapped L8 "Thank You, Thank You" → [0,1,2,3], end_ms=57662.
    // Claude mapped L9 "For the wonders You've done" → [4,5,...], start_ms=57682.
    //
    // Without the fix: L9.start_ms=57682 (only 20ms after L8.end=57662).
    // Phase 5 small-gap extension then sets L8.end=57682.
    // L8 window = 55960→57682 = 1722ms — second "Thank You" still being sung.
    //
    // With the fix: idx=4 "for" (dur=60ms, conf=0.0) is a start-artefact.
    // Gap from idx=4.end=57742 to idx=5.start=60344 = 2602ms > threshold.
    // emit_single uses idx=5.start_ms=60344 for L9.start_ms.
    // Phase 5 small-gap rule then extends L8.end=57662→60344 (gap=2682ms ≤ 4000ms).
    // L8 window = 55960→60344 = 4384ms ✓ covers both "Thank You" tokens.
    let asr_track = asr(vec![
        make_word_with_conf("thank", 55960, 56921, 0.844),
        make_word_with_conf("you", 56981, 57282, 0.732),
        make_word_with_conf("thank", 57302, 57582, 0.174),
        make_word_with_conf("you", 57602, 57662, 0.003), // 60ms, near-zero conf
        make_word_with_conf("for", 57682, 57742, 0.0),   // 60ms, zero conf, artefact
        make_word_with_conf("the", 60344, 61045, 0.85),  // real start of next line
        make_word_with_conf("wonders", 61105, 61405, 0.85),
    ]);
    let asr_words = flatten_asr(&asr_track);
    // L9 "For the wonders" gets indices [4,5,6] from Phase 1 (Claude).
    let emit_l9 = LineEmit {
        text: "For the wonders".into(),
        asr_word_indices: vec![4, 5, 6],
    };
    let lines = aligned_lines_for_emit(&emit_l9, &asr_words, None);
    assert_eq!(lines.len(), 1);
    // start_ms must skip the 60ms zero-conf artefact "for" and land on
    // the real first word "the" at 60344ms.
    assert_eq!(
        lines[0].start_ms, 60344,
        "emit_single must skip near-zero-conf 60ms start artefact; got start_ms={}",
        lines[0].start_ms
    );
    assert_eq!(lines[0].end_ms, 61405);
}

#[test]
fn emit_single_keeps_normal_first_word_when_not_artifact() {
    // Normal case: first word has normal confidence and duration — no skip.
    let asr_track = asr(vec![
        make_word_with_conf("for", 1000, 1200, 0.8), // normal duration + conf
        make_word_with_conf("the", 1300, 1600, 0.9),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "for the".into(),
        asr_word_indices: vec![0, 1],
    };
    let lines = aligned_lines_for_emit(&emit, &asr_words, None);
    assert_eq!(
        lines[0].start_ms, 1000,
        "normal first word must not be skipped"
    );
}

#[test]
fn emit_single_no_skip_when_gap_to_second_word_is_small() {
    // Even if first word is short+low-conf, no skip when second word starts
    // within START_ARTIFACT_GAP_MS — they are adjacent tokens, not a gap.
    let asr_track = asr(vec![
        make_word_with_conf("for", 57682, 57742, 0.0), // 60ms, zero conf
        make_word_with_conf("the", 57800, 58500, 0.9), // only 58ms gap → no skip
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "for the".into(),
        asr_word_indices: vec![0, 1],
    };
    let lines = aligned_lines_for_emit(&emit, &asr_words, None);
    assert_eq!(
        lines[0].start_ms, 57682,
        "should not skip first word when second is close (no gap)"
    );
}

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
