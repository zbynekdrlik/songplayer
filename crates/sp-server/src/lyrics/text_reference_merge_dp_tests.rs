//! Tests for the Phase-1 NW-DP `match_ref_to_asr` function. Sibling-
//! included from description_merge.rs to keep description_merge_tests.rs
//! under the 1000-line file-size cap.

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

#[test]
fn match_ref_to_asr_returns_empty_when_no_asr_words() {
    // m == 0 (asr empty) — kills part of `n == 0 || m == 0` (line 305:15).
    let ref_lines = vec!["holy is the lord".to_string()];
    let asr_track = asr(vec![]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 1);
    assert!(emits[0].asr_word_indices.is_empty());
}

#[test]
fn match_ref_to_asr_returns_empty_when_no_ref_words() {
    // n == 0 (ref empty after flatten) — second arm of `n == 0 || m == 0`.
    let ref_lines: Vec<String> = vec![];
    let asr_track = asr(vec![make_word("a", 0, 100)]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert!(emits.is_empty());
}

#[test]
fn match_ref_to_asr_skips_unmatched_asr_words_to_find_matches() {
    // ref = "a b", asr = [a, x, b] — DP must skip "x" to align both
    // refs to their asr matches. Indices [0, 2].
    let ref_lines = vec!["a b".to_string()];
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("x", 200, 300),
        make_word("b", 400, 500),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 1);
    assert_eq!(emits[0].asr_word_indices, vec![0, 2]);
}

#[test]
fn match_ref_to_asr_tracks_through_intro_silence() {
    // dp[0][j] = 0 boundary (intro skip is free). asr starts with 3
    // unmatched words BEFORE the first ref word lands. DP must keep all
    // those words unmatched and find the real matches at [3, 4].
    let ref_lines = vec!["hello world".to_string()];
    let asr_track = asr(vec![
        make_word("intro", 0, 100),
        make_word("intro2", 200, 300),
        make_word("intro3", 400, 500),
        make_word("hello", 600, 700),
        make_word("world", 800, 900),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 1);
    assert_eq!(
        emits[0].asr_word_indices,
        vec![3, 4],
        "intro words must be skipped (free) and real matches found"
    );
}

#[test]
fn match_ref_to_asr_assigns_two_word_ref_across_two_lines() {
    // Multi-line ref with matches in interleaved asr. Each line gets its
    // own indices vector. Verifies the line_idx tagging in ref_pairs.
    let ref_lines = vec!["a b".to_string(), "c d".to_string()];
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 300),
        make_word("c", 1000, 1100),
        make_word("d", 1200, 1300),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 2);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1]);
    assert_eq!(emits[1].asr_word_indices, vec![2, 3]);
}

#[test]
fn match_ref_to_asr_returns_empty_match_for_disjoint_words() {
    // ref = "x y", asr = [a, b, c]. No matches possible. DP picks all-skip.
    let ref_lines = vec!["x y".to_string()];
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 300),
        make_word("c", 400, 500),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    assert_eq!(emits.len(), 1);
    assert!(
        emits[0].asr_word_indices.is_empty(),
        "no matching words must yield empty indices"
    );
}
