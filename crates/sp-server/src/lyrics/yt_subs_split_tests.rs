//! Unit tests for `anchor_subs_to_window`. The async outer
//! `split_long_line_with_anchors` requires a wired Claude proxy and is
//! exercised end-to-end on win-resolume reprocess verification.

#![allow(unused_imports)]

use super::{anchor_subs_to_window, cluster_caption_windows};
use crate::lyrics::backend::{AlignedLine, AlignedWord};

fn aw(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.into(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

#[test]
fn anchors_first_start_and_last_end_to_yt_subs_times() {
    // yt_subs line: "Thank You for today That You have made"
    // start=8090, end=11380. Claude split: ["Thank You for today",
    // "That You have made"]. ASR has all words inside the yt_subs
    // window. Sub-1 starts at first whisperx word "thank" — which IS
    // 8090 (= yt_subs anchor, win), Sub-2 starts at whisperx "that"
    // 9260. Sub-1.end = sub-2.start = 9260. Sub-2.end = yt_subs end
    // 11380.
    let asr = vec![
        aw("thank", 8090, 8400),
        aw("you", 8400, 8600),
        aw("for", 8600, 8800),
        aw("today", 8900, 9200),
        aw("that", 9260, 9500),
        aw("you", 9500, 9700),
        aw("have", 9700, 9900),
        aw("made", 10000, 11380),
    ];
    let subs = vec![
        "Thank You for today".to_string(),
        "That You have made".to_string(),
    ];
    let result = anchor_subs_to_window(&subs, 8090, 11380, &asr).expect("should succeed");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].text, "Thank You for today");
    assert_eq!(
        result[0].start_ms, 8090,
        "yt_subs anchor at first sub start"
    );
    assert_eq!(
        result[0].end_ms, 9260,
        "boundary = whisperx start of next sub"
    );
    assert_eq!(result[1].text, "That You have made");
    assert_eq!(result[1].start_ms, 9260);
    assert_eq!(result[1].end_ms, 11380, "yt_subs anchor at last sub end");
}

#[test]
fn returns_none_when_sub_count_is_one() {
    let asr = vec![aw("alpha", 0, 1000)];
    let subs = vec!["alpha".to_string()];
    assert!(anchor_subs_to_window(&subs, 0, 1000, &asr).is_none());
}

#[test]
fn returns_none_when_window_has_no_asr_words() {
    let asr = vec![aw("outside", 100, 200)];
    let subs = vec!["alpha".to_string(), "beta".to_string()];
    // window is 5000-10000, no asr in range
    assert!(anchor_subs_to_window(&subs, 5000, 10000, &asr).is_none());
}

#[test]
fn returns_none_when_sub_lcs_fails_against_window() {
    // Sub words don't appear in window — LCS finds no match.
    let asr = vec![
        aw("foo", 0, 500),
        aw("bar", 500, 1000),
        aw("baz", 1000, 1500),
    ];
    let subs = vec!["alpha beta".to_string(), "gamma delta".to_string()];
    assert!(anchor_subs_to_window(&subs, 0, 1500, &asr).is_none());
}

#[test]
fn preserves_anchors_even_when_first_whisperx_word_is_late() {
    // First whisperx word starts AFTER yt_subs.start (singer's first
    // syllable detection lagged). yt_subs.start MUST still anchor sub[0].
    let asr = vec![
        aw("alpha", 100, 300),
        aw("beta", 400, 600),
        aw("gamma", 700, 900),
        aw("delta", 1000, 1200),
    ];
    let subs = vec!["alpha beta".to_string(), "gamma delta".to_string()];
    let result = anchor_subs_to_window(&subs, 0, 1500, &asr).expect("should succeed");
    assert_eq!(
        result[0].start_ms, 0,
        "yt_subs anchor wins over whisperx 100"
    );
    assert_eq!(result[0].end_ms, 700, "boundary at gamma start");
    assert_eq!(result[1].start_ms, 700);
    assert_eq!(
        result[1].end_ms, 1500,
        "yt_subs anchor wins over whisperx 1200"
    );
}

#[test]
fn returns_none_when_boundary_would_not_be_monotonic() {
    // Pathological case: two subs map to the SAME first whisperx word.
    // Boundary computation would yield zero-duration. Reject.
    let asr = vec![aw("only", 100, 500)];
    let subs = vec!["only".to_string(), "only".to_string()];
    let result = anchor_subs_to_window(&subs, 0, 1000, &asr);
    assert!(
        result.is_none(),
        "subs sharing the same first whisperx word must trigger fallback"
    );
}

fn al(text: &str, s: u32, e: u32) -> AlignedLine {
    AlignedLine {
        text: text.into(),
        start_ms: s,
        end_ms: e,
        words: None,
    }
}

#[test]
fn cluster_merges_caption_window_adjacent_lines() {
    // id=232 'Praise God' yt_subs sample: lines 0-4 are back-to-back
    // (caption-window adjacent). Real sentence end after "given up" has
    // a 1 s gap so a new cluster starts there.
    let lines = vec![
        al("Thank You for", 8090, 9260),
        al("today  That You have made", 9260, 11380),
        al("I give You all", 11380, 12370),
        al("the glory  And I", 12370, 13460),
        al("give You all the praise", 13460, 15170),
        al("Thank You for the", 15170, 16600),
        al("breath  Inside my lungs", 16600, 18550),
        al("Thank You for Your", 18550, 20000),
        al("grace  That's never given up", 20000, 22220),
        // 1 s pause — new cluster
        al("For God so loved", 23220, 24200),
        al("the world  That", 24200, 25110),
    ];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 2, "expected 2 phrase clusters");
    assert_eq!(merged[0].start_ms, 8090);
    assert_eq!(merged[0].end_ms, 22220);
    assert_eq!(
        merged[0].text,
        "Thank You for today That You have made I give You all the glory And I give You all the praise Thank You for the breath Inside my lungs Thank You for Your grace That's never given up"
    );
    assert_eq!(merged[1].start_ms, 23220);
    assert_eq!(merged[1].end_ms, 25110);
    assert_eq!(merged[1].text, "For God so loved the world That");
}

#[test]
fn cluster_keeps_lines_with_real_pause_between_them() {
    // 500ms gap = real phrase pause; do NOT merge.
    let lines = vec![al("First line", 0, 1000), al("Second line", 1500, 2500)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].text, "First line");
    assert_eq!(merged[1].text, "Second line");
}

#[test]
fn cluster_normalizes_double_whitespace_in_merged_text() {
    // yt_subs lines often contain stray double-spaces; merging must
    // collapse to single spaces.
    let lines = vec![al("a  b", 0, 100), al("c  d", 100, 200)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].text, "a b c d");
}

#[test]
fn cluster_merges_overlapping_lines_too() {
    // line[i+1].start_ms < line[i].end_ms (overlapping caption windows).
    let lines = vec![al("Hello", 0, 1500), al("world", 1000, 2000)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].start_ms, 0);
    assert_eq!(merged[0].end_ms, 2000);
}
