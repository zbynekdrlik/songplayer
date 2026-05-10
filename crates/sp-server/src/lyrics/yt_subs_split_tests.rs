//! Unit tests for `cluster_caption_windows`. Pre-step before the
//! orchestrator routes yt_subs through text_reference_merge.

#![allow(unused_imports)]

use super::cluster_caption_windows;
use crate::lyrics::backend::AlignedLine;

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
    assert_eq!(merged[0].start_ms, 0);
    assert_eq!(merged[0].end_ms, 2000);
}
