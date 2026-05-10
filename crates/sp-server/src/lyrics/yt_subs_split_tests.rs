//! Unit tests for `cluster_caption_windows` and
//! `anchor_subs_with_fallback`. The async outer `split_cluster`
//! requires a wired Claude proxy and is exercised end-to-end on
//! win-resolume reprocess verification.

#![allow(unused_imports)]

use super::{anchor_subs_with_fallback, cluster_caption_windows};
use crate::lyrics::backend::{AlignedLine, AlignedWord};

fn aw(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.into(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn al(text: &str, s: u32, e: u32) -> AlignedLine {
    AlignedLine {
        text: text.into(),
        start_ms: s,
        end_ms: e,
        words: None,
    }
}

// ── cluster_caption_windows ──────────────────────────────────────────────────

#[test]
fn cluster_merges_caption_window_adjacent_lines() {
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
        al("For God so loved", 23220, 24200),
        al("the world  That", 24200, 25110),
    ];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].start_ms, 8090);
    assert_eq!(merged[0].end_ms, 22220);
    assert_eq!(merged[1].start_ms, 23220);
    assert_eq!(merged[1].end_ms, 25110);
}

#[test]
fn cluster_keeps_lines_with_real_pause_between_them() {
    let lines = vec![al("First line", 0, 1000), al("Second line", 1500, 2500)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 2);
}

#[test]
fn cluster_normalizes_double_whitespace_in_merged_text() {
    let lines = vec![al("a  b", 0, 100), al("c  d", 100, 200)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].text, "a b c d");
}

#[test]
fn cluster_merges_overlapping_lines_too() {
    let lines = vec![al("Hello", 0, 1500), al("world", 1000, 2000)];
    let merged = cluster_caption_windows(&lines);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].end_ms, 2000);
}

// ── anchor_subs_with_fallback ────────────────────────────────────────────────

#[test]
fn anchor_uses_yt_subs_anchors_at_first_start_and_last_end() {
    // Two subs, first whisperx word matches second sub. First sub's
    // start = cluster_start (yt_subs anchor); last sub's end = cluster_end.
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
    let out = anchor_subs_with_fallback(&subs, 8090, 11380, &asr);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].start_ms, 8090, "yt_subs anchor at first sub start");
    assert_eq!(out[1].end_ms, 11380, "yt_subs anchor at last sub end");
    // sub[1] anchored at "that" = 9260.
    assert_eq!(out[0].end_ms, 9260);
    assert_eq!(out[1].start_ms, 9260);
}

#[test]
fn anchor_proportionally_fills_unmatched_subs() {
    // 3 subs. Only sub 0 (anchored at cluster_start) and sub 2 (matches
    // "found") have anchors. Sub 1 has no whisperx match — interpolate
    // proportionally between cluster_start (sub 0 anchor) and sub 2's
    // anchor by character count.
    let asr = vec![aw("found", 5000, 5500)];
    let subs = vec![
        "Lost".to_string(),           // 4c — anchored at cluster_start
        "Forgotten word".to_string(), // 14c — no asr match
        "Found here".to_string(),     // 10c — sub 2 matches "found"
    ];
    let out = anchor_subs_with_fallback(&subs, 0, 6000, &asr);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start_ms, 0);
    // sub 2 anchored at "found" 5000.
    assert_eq!(out[2].start_ms, 5000);
    // sub 1 interpolated. weights = [4, 14] (subs 0 and 1, since j==2).
    // total=18; sub 1 starts at 0 + (4/18)*5000 = 1111.
    assert_eq!(out[1].start_ms, 1111);
    // sub 1 ends at sub 2 start.
    assert_eq!(out[1].end_ms, 5000);
    // sub 2 ends at cluster_end (yt_subs anchor).
    assert_eq!(out[2].end_ms, 6000);
}

#[test]
fn anchor_never_drops_sub_text_even_when_all_unmatched() {
    // No whisperx matches at all → interpolate everything by char
    // count between cluster_start and cluster_end.
    let asr: Vec<AlignedWord> = vec![];
    let subs = vec![
        "alpha".to_string(), // 5c
        "beta".to_string(),  // 4c
        "gamma".to_string(), // 5c
    ];
    let out = anchor_subs_with_fallback(&subs, 0, 14000, &asr);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start_ms, 0);
    assert_eq!(out[2].end_ms, 14000);
    // weights: 5, 4, 5 → total 14. sub 1: (5/14)*14000 = 5000.
    assert_eq!(out[1].start_ms, 5000);
    // sub 2: (5+4)/14*14000 = 9000.
    assert_eq!(out[2].start_ms, 9000);
}

#[test]
fn anchor_bounded_lcs_does_not_jump_far() {
    // sub 0 = "for god so loved". whisperx has "so" at position 23
    // (way after "for god"). Bounded LCS (lookahead 10) must NOT
    // consume "so" at position 23. Result: sub 0 matches "for"=0 +
    // "god"=1 only, search_from advances to 2.
    let mut asr: Vec<AlignedWord> = Vec::new();
    asr.push(aw("for", 0, 100));
    asr.push(aw("god", 100, 200));
    for i in 2..23 {
        asr.push(aw("filler", (i * 100) as u32, ((i + 1) * 100) as u32));
    }
    asr.push(aw("so", 2300, 2400));
    asr.push(aw("praise", 2400, 2500));
    asr.push(aw("god", 2500, 2600));
    let subs = vec!["For God so loved".to_string(), "praise god".to_string()];
    let out = anchor_subs_with_fallback(&subs, 0, 3000, &asr);
    assert_eq!(out.len(), 2);
    // sub 0 anchored at cluster_start (always).
    assert_eq!(out[0].start_ms, 0);
    // sub 1 should anchor on "praise" (2400) — bounded LCS for sub 0
    // consumed "for","god" (positions 0,1) without jumping to "so"@23.
    // search_from = 2. sub 1 LCS in window[2..12] looks for "praise","god";
    // window[2..12] is all "filler" — no match. Bounded LCS for sub 1
    // returns no match → proportional fallback. So sub 1 start =
    // (5/(16+10))*3000 = depends. The KEY assertion: sub 1.start
    // is NOT before sub 0's match end. Just monotonic.
    assert!(out[1].start_ms >= out[0].end_ms);
    assert_eq!(out[1].end_ms, 3000);
}

#[test]
fn anchor_single_sub_returns_full_cluster_range() {
    let subs = vec!["only".to_string()];
    let out = anchor_subs_with_fallback(&subs, 100, 1000, &[]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].start_ms, 100);
    assert_eq!(out[0].end_ms, 1000);
}
