//! Tests for Phase 2.5 trim_outlier_indices.
//! Sibling-included from text_reference_merge.rs.

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
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);
}

#[test]
fn trim_outlier_indices_drops_to_single_when_two_entry_span_exceeds_cap() {
    let asr_track = asr(vec![make_word("a", 0, 100), make_word("b", 50000, 50100)]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_keeps_single_entry_intact() {
    let asr_track = asr(vec![make_word("a", 1000, 1500)]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_pops_outlier_when_max_gap_is_in_middle() {
    // id=21 4:05: 15 s held vowel between "house" and "of"; last pair
    // gap is small but middle gap is the outlier.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 400),
        make_word("c", 500, 700),
        make_word("d", 16000, 16200),
        make_word("e", 16300, 16500),
        make_word("f", 16600, 17000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = (0..6).collect::<Vec<_>>();
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0, 1, 2]);
}

#[test]
fn trim_outlier_indices_keeps_contiguous_held_notes_past_cap() {
    // id=21 2:00: held notes; span > cap but every gap < 3 s. Keep whole.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 1000, 1500),
        make_word("c", 2500, 3000),
        make_word("d", 4000, 4500),
        make_word("e", 5500, 6000),
        make_word("f", 7000, 8000),
        make_word("g", 8500, 12500),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = (0..7).collect::<Vec<_>>();
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, (0..7).collect::<Vec<_>>());
}

#[test]
fn trim_outlier_indices_handles_unsorted_input() {
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
