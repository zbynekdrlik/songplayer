//! Unit tests for `anchor_subs_with_fallback`. The async outer
//! `split_cluster` requires a wired Claude proxy and is exercised
//! end-to-end on win-resolume reprocess verification.

#![allow(unused_imports)]

use super::anchor_subs_with_fallback;
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
fn anchor_uses_yt_subs_anchors_at_first_start_and_last_end() {
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
    assert_eq!(out[0].end_ms, 9260);
    assert_eq!(out[1].start_ms, 9260);
}

#[test]
fn anchor_proportionally_fills_unmatched_subs() {
    // 3 subs: weights 4, 14, 10 (total 28). cluster_dur=6000.
    // prop_start = [0, 857, 3857]. Whisperx "found"@5000 for sub 2:
    // |5000-3857|=1143; tolerance=(6000-3857)/2=1071. 1143>1071 →
    // whisperx anchor REJECTED, use proportional 3857.
    let asr = vec![aw("found", 5000, 5500)];
    let subs = vec![
        "Lost".to_string(),
        "Forgotten word".to_string(),
        "Found here".to_string(),
    ];
    let out = anchor_subs_with_fallback(&subs, 0, 6000, &asr);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start_ms, 0);
    assert_eq!(out[1].start_ms, 857);
    assert_eq!(out[2].start_ms, 3857);
    assert_eq!(out[2].end_ms, 6000);
}

#[test]
fn anchor_accepts_whisperx_when_within_proportional_tolerance() {
    // Sub 2 prop=3857, whisperx@4000: |4000-3857|=143 < tolerance 1071.
    // Whisperx anchor accepted.
    let asr = vec![aw("found", 4000, 4500)];
    let subs = vec![
        "Lost".to_string(),
        "Forgotten word".to_string(),
        "Found here".to_string(),
    ];
    let out = anchor_subs_with_fallback(&subs, 0, 6000, &asr);
    assert_eq!(out[2].start_ms, 4000);
}

#[test]
fn anchor_never_drops_sub_text_even_when_all_unmatched() {
    let asr: Vec<AlignedWord> = vec![];
    let subs = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
    let out = anchor_subs_with_fallback(&subs, 0, 14000, &asr);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start_ms, 0);
    assert_eq!(out[2].end_ms, 14000);
    assert_eq!(out[1].start_ms, 5000);
    assert_eq!(out[2].start_ms, 9000);
}

#[test]
fn anchor_bounded_lcs_does_not_jump_far() {
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
    assert_eq!(out[0].start_ms, 0);
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
