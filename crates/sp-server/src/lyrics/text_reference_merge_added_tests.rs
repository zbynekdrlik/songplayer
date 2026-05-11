//! Tests for Phase 1.5 align_added_lines.
//! Sibling-included from text_reference_merge_added.rs.

#![allow(unused_imports)]

use super::super::mapping::AddedRefLine;
use super::*;

fn word(text: &str, start_ms: u32, end_ms: u32) -> AsrWord {
    AsrWord {
        norm: super::super::normalize_word(text),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn empty_emit(text: &str) -> LineEmit {
    LineEmit {
        text: text.into(),
        asr_word_indices: vec![],
    }
}

#[test]
fn align_added_lines_matches_words_in_unmatched_window() {
    // Verse @ 0-2, added line @ expanded idx 1 (after_line=0), Next @ 9.
    let asr = vec![
        word("verse", 0, 100),
        word("verse", 200, 400),
        word("verse", 500, 700),
        word("theres", 1000, 1200),
        word("no", 1300, 1400),
        word("place", 1500, 1700),
        word("id", 1800, 1900),
        word("rather", 2000, 2200),
        word("be", 2300, 2500),
        word("next", 5000, 5200),
    ];
    let expanded_ref = vec![
        "Verse line".to_string(),
        "There's no place I'd rather be".to_string(),
        "Next line".to_string(),
    ];
    let existing = vec![
        LineEmit {
            text: "Verse line".into(),
            asr_word_indices: vec![0, 1, 2],
        },
        empty_emit("There's no place I'd rather be"),
        LineEmit {
            text: "Next line".into(),
            asr_word_indices: vec![9],
        },
    ];
    let added = vec![AddedRefLine {
        after_line: 0,
        text: "There's no place I'd rather be".into(),
    }];
    let expanded_indices = vec![1usize];
    let orig_to_expanded = vec![0usize, 2usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "There's no place I'd rather be");
    assert!(!out[0].asr_word_indices.is_empty());
    let mn = *out[0].asr_word_indices.iter().min().unwrap();
    let mx = *out[0].asr_word_indices.iter().max().unwrap();
    assert!(
        mn >= 3 && mx <= 8,
        "indices in window 3..=8: got {:?}",
        out[0].asr_word_indices
    );
}

#[test]
fn align_added_lines_skips_when_window_empty() {
    // A @ 0, B @ 1 — no room between them.
    let asr = vec![word("a", 0, 100), word("b", 200, 400)];
    let expanded_ref = vec!["A".to_string(), "ADDED".to_string(), "B".to_string()];
    let existing = vec![
        LineEmit {
            text: "A".into(),
            asr_word_indices: vec![0],
        },
        empty_emit("ADDED"),
        LineEmit {
            text: "B".into(),
            asr_word_indices: vec![1],
        },
    ];
    let added = vec![AddedRefLine {
        after_line: 0,
        text: "ADDED".into(),
    }];
    let expanded_indices = vec![1usize];
    let orig_to_expanded = vec![0usize, 2usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 1);
    assert!(out[0].asr_word_indices.is_empty());
}

#[test]
fn align_added_lines_respects_after_line_ordering() {
    // Two added lines with same after_line: both fit between desc and end.
    let asr = vec![
        word("desc", 0, 100),
        word("first", 1000, 1200),
        word("added", 1300, 1500),
        word("line", 1600, 1800),
        word("second", 2000, 2200),
        word("added", 2300, 2500),
        word("line", 2600, 2800),
        word("end", 5000, 5200),
    ];
    let expanded_ref = vec![
        "Desc".to_string(),
        "First added line".to_string(),
        "Second added line".to_string(),
        "End".to_string(),
    ];
    let existing = vec![
        LineEmit {
            text: "Desc".into(),
            asr_word_indices: vec![0],
        },
        empty_emit("First added line"),
        empty_emit("Second added line"),
        LineEmit {
            text: "End".into(),
            asr_word_indices: vec![7],
        },
    ];
    let added = vec![
        AddedRefLine {
            after_line: 0,
            text: "First added line".into(),
        },
        AddedRefLine {
            after_line: 0,
            text: "Second added line".into(),
        },
    ];
    let expanded_indices = vec![1usize, 2usize];
    let orig_to_expanded = vec![0usize, 3usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 2);
    assert!(!out[0].asr_word_indices.is_empty() && !out[1].asr_word_indices.is_empty());
    let first_max = *out[0].asr_word_indices.iter().max().unwrap();
    let second_min = *out[1].asr_word_indices.iter().min().unwrap();
    assert!(first_max < second_min);
}

#[test]
fn align_added_lines_handles_partial_lcs_match() {
    let asr = vec![
        word("desc", 0, 100),
        word("the", 1000, 1100),
        word("ankle", 1200, 1400),
        word("of", 1500, 1600),
        word("my", 1700, 1800),
        word("hope", 1900, 2100),
        word("next", 5000, 5200),
    ];
    let expanded_ref = vec![
        "Desc".to_string(),
        "The anchor of my hope".to_string(),
        "Next".to_string(),
    ];
    let existing = vec![
        LineEmit {
            text: "Desc".into(),
            asr_word_indices: vec![0],
        },
        empty_emit("The anchor of my hope"),
        LineEmit {
            text: "Next".into(),
            asr_word_indices: vec![6],
        },
    ];
    let added = vec![AddedRefLine {
        after_line: 0,
        text: "The anchor of my hope".into(),
    }];
    let expanded_indices = vec![1usize];
    let orig_to_expanded = vec![0usize, 2usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 1);
    assert!(out[0].asr_word_indices.len() >= 3);
}

#[test]
fn align_added_lines_uses_full_song_tail_when_after_last_line() {
    let asr = vec![
        word("desc", 0, 100),
        word("you", 5000, 5200),
        word("are", 5300, 5500),
        word("my", 5600, 5800),
        word("rock", 5900, 6300),
    ];
    let expanded_ref = vec!["Desc".to_string(), "You are my rock".to_string()];
    let existing = vec![
        LineEmit {
            text: "Desc".into(),
            asr_word_indices: vec![0],
        },
        empty_emit("You are my rock"),
    ];
    let added = vec![AddedRefLine {
        after_line: 0,
        text: "You are my rock".into(),
    }];
    let expanded_indices = vec![1usize];
    let orig_to_expanded = vec![0usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 1);
    assert!(out[0].asr_word_indices.len() >= 3);
    assert!(*out[0].asr_word_indices.iter().max().unwrap() == 4);
}

#[test]
fn align_added_lines_anchors_to_after_line_not_song_start() {
    // Regression for id=21 2026-05-08: added line with after_line=1 must
    // search ASR window AFTER description line 1's audio, not start of
    // song. Without this anchor, "You are my rock" added for outro can
    // match "you" word in verse 1 and emit at wrong time.
    let asr = vec![
        word("you", 0, 100), // verse 1 "you" (early in song)
        word("make", 200, 400),
        word("are", 500, 700),   // verse 1 "are"
        word("you", 5000, 5100), // outro "you" (late)
        word("are", 5200, 5300),
        word("my", 5400, 5500),
        word("rock", 5600, 5800),
    ];
    let expanded_ref = vec![
        "You make".to_string(),
        "Are you".to_string(),
        "You are my rock".to_string(),
    ];
    let existing = vec![
        LineEmit {
            text: "You make".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Are you".into(),
            asr_word_indices: vec![2],
        },
        empty_emit("You are my rock"),
    ];
    let added = vec![AddedRefLine {
        after_line: 1,
        text: "You are my rock".into(),
    }];
    let expanded_indices = vec![2usize];
    let orig_to_expanded = vec![0usize, 1usize];
    let out = align_added_lines(
        &expanded_ref,
        &added,
        &expanded_indices,
        &orig_to_expanded,
        &asr,
        &existing,
    );
    assert_eq!(out.len(), 1);
    assert!(!out[0].asr_word_indices.is_empty());
    let mn = *out[0].asr_word_indices.iter().min().unwrap();
    assert!(
        mn >= 3,
        "must anchor AFTER 'Are you' emit (idx 2); got min={mn}"
    );
}
