//! yt_subs anchored Claude split with whisperx internal anchors and
//! proportional fallback.
//!
//! YouTube auto-subs ship line-level timing that is generally accurate;
//! the line BREAKS reflect the caption-display window so they split
//! mid-phrase ("Thank You for / today That You have made"). For
//! karaoke-quality wall display long yt_subs lines are re-broken at
//! natural phrase boundaries via Claude. yt_subs is the AUTHORITY for
//! what is sung; whisperx is consulted only to refine internal sub-line
//! timing where its words match the sub text.
//!
//! Algorithm per yt_subs phrase cluster (post `cluster_caption_windows`):
//!
//! 1. If text fits the 32-char karaoke cap → emit as-is with yt_subs
//!    timing.
//! 2. Else, Claude-split into N sub-lines (`claude_split_lines` +
//!    deterministic fallback).
//! 3. For each sub, BOUNDED-LCS its words against the next 10 whisperx
//!    words after the previous sub's last match. Bounded lookahead
//!    prevents the matcher from jumping far ahead when a stray ref
//!    word ("so") happens to reappear later in whisperx.
//! 4. Anchor subs that found whisperx matches at the matched start
//!    time. For subs where bounded LCS found no match (whisperx missed
//!    those words OR mistranscribed them), interpolate the start time
//!    PROPORTIONALLY between the surrounding anchored subs by character
//!    count. yt_subs sub texts always ship — never dropped.
//! 5. yt_subs anchors win at the first sub's start (cluster.start_ms)
//!    and the last sub's end (cluster.end_ms).

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignedLine, AlignedWord};
use crate::lyrics::text_reference_merge::{
    SUBLINE_MAX_CHARS, claude_split_lines, deterministic_split_one, lcs_align, normalize_word,
};
use std::collections::HashMap;
use tracing::warn;

/// Bounded LCS lookahead per sub. Larger → matcher can find legitimate
/// matches that drift; smaller → blocks ref-word reappearance from
/// pulling search_from far ahead. id=232 cluster 2: "so" reappeared
/// 23 positions later in whisperx after "For God"; with bound=10 the
/// matcher stops before reaching it and search_from stays correct.
const LCS_BOUND_WORDS: usize = 10;

/// Cluster YouTube caption-window adjacent yt_subs lines back into
/// real phrases. Adjacent (gap == 0 or overlapping) lines merge;
/// non-adjacent (gap > 0 = real phrase pause) stay split. Whitespace
/// is normalized to single spaces.
pub(crate) fn cluster_caption_windows(lines: &[AlignedLine]) -> Vec<AlignedLine> {
    let mut out: Vec<AlignedLine> = Vec::with_capacity(lines.len());
    for line in lines {
        if let Some(last) = out.last_mut() {
            if last.end_ms >= line.start_ms {
                let merged = format!("{} {}", last.text.trim(), line.text.trim());
                last.text = merged.split_whitespace().collect::<Vec<_>>().join(" ");
                last.end_ms = line.end_ms;
                continue;
            }
        }
        out.push(line.clone());
    }
    out
}

/// Re-break a single yt_subs phrase cluster using Claude phrasing,
/// bounded whisperx LCS for internal anchors, and proportional
/// interpolation for subs whisperx missed.
pub(crate) async fn split_cluster(
    ai_client: &AiClient,
    text: &str,
    cluster_start_ms: u32,
    cluster_end_ms: u32,
    asr_words: &[AlignedWord],
) -> Vec<AlignedLine> {
    let unsplit = || {
        vec![AlignedLine {
            text: text.to_string(),
            start_ms: cluster_start_ms,
            end_ms: cluster_end_ms,
            words: None,
        }]
    };
    if text.chars().count() <= SUBLINE_MAX_CHARS {
        return unsplit();
    }

    let lines_in = vec![(0usize, text)];
    let mut split_map: HashMap<usize, Vec<String>> = claude_split_lines(ai_client, &lines_in)
        .await
        .unwrap_or_default();
    let sub_texts = match split_map.remove(&0) {
        Some(s) if s.len() > 1 => s,
        _ => deterministic_split_one(text),
    };
    if sub_texts.len() <= 1 {
        return unsplit();
    }

    anchor_subs_with_fallback(&sub_texts, cluster_start_ms, cluster_end_ms, asr_words)
}

/// Anchor pre-split sub-line texts to whisperx word boundaries inside
/// the yt_subs cluster's `[cluster_start_ms, cluster_end_ms]` window.
/// Subs whose bounded LCS finds a whisperx anchor get the matched
/// word's start_ms; subs that find no anchor are interpolated
/// proportionally by character count between surrounding anchored subs.
/// First sub starts at cluster_start_ms, last sub ends at
/// cluster_end_ms (yt_subs authority preserved at the boundary).
pub(crate) fn anchor_subs_with_fallback(
    sub_texts: &[String],
    cluster_start_ms: u32,
    cluster_end_ms: u32,
    asr_words: &[AlignedWord],
) -> Vec<AlignedLine> {
    let n = sub_texts.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![AlignedLine {
            text: sub_texts[0].clone(),
            start_ms: cluster_start_ms,
            end_ms: cluster_end_ms,
            words: None,
        }];
    }

    // Whisperx words overlapping the yt_subs cluster.
    let window: Vec<&AlignedWord> = asr_words
        .iter()
        .filter(|w| w.end_ms > cluster_start_ms && w.start_ms < cluster_end_ms)
        .collect();
    let window_norms: Vec<String> = window.iter().map(|w| normalize_word(&w.text)).collect();
    let window_strs: Vec<&str> = window_norms.iter().map(|s| s.as_str()).collect();

    // Bounded LCS per sub. search_from advances only when a sub
    // matches within bound words; otherwise it stays put so subsequent
    // subs still have access to the same window region.
    let mut anchor_w_idx: Vec<Option<usize>> = Vec::with_capacity(n);
    let mut search_from = 0usize;
    for sub_text in sub_texts {
        let sub_norms: Vec<String> = sub_text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        if sub_norms.is_empty() || search_from >= window_strs.len() {
            anchor_w_idx.push(None);
            continue;
        }
        let sub_strs: Vec<&str> = sub_norms.iter().map(|s| s.as_str()).collect();
        let bound_end = (search_from + LCS_BOUND_WORDS).min(window_strs.len());
        let sub_window = &window_strs[search_from..bound_end];
        let alignment = lcs_align(&sub_strs, sub_window);
        let matched: Vec<usize> = alignment.iter().filter_map(|a| *a).collect();
        if matched.is_empty() {
            anchor_w_idx.push(None);
            // search_from unchanged.
            continue;
        }
        let first = *matched.first().expect("non-empty");
        let last = *matched.last().expect("non-empty");
        anchor_w_idx.push(Some(search_from + first));
        search_from += last + 1;
    }

    // Build per-sub start_ms list.
    let mut start_ms: Vec<u32> = vec![0; n];
    start_ms[0] = cluster_start_ms;
    for i in 1..n {
        match anchor_w_idx[i] {
            Some(w_idx) => start_ms[i] = window[w_idx].start_ms,
            None => start_ms[i] = 0, // placeholder, fill below
        }
    }

    // Proportional fallback for unanchored subs. Walk i from 1..n;
    // if start_ms[i] == 0 (unanchored), find the next anchored
    // index j (or n→cluster_end) and interpolate starts for i..j by
    // character counts of subs i..j-1.
    let mut i = 1;
    while i < n {
        if start_ms[i] != 0 {
            i += 1;
            continue;
        }
        // Run of unanchored subs from i to next_anchored-1.
        let mut j = i;
        while j < n && start_ms[j] == 0 {
            j += 1;
        }
        // Bracket: prev anchor at i-1, next anchor at j (or cluster_end if j == n).
        let prev_ms = start_ms[i - 1];
        let next_ms = if j == n { cluster_end_ms } else { start_ms[j] };
        if next_ms <= prev_ms {
            // No room — collapse all unanchored subs to prev_ms.
            for k in i..j {
                start_ms[k] = prev_ms;
            }
        } else {
            // Distribute by char counts of the BRACKETED subs (i-1..j or i-1..n-1).
            let last_bracketed = if j == n { n - 1 } else { j - 1 };
            let weights: Vec<usize> = (i - 1..=last_bracketed)
                .map(|k| sub_texts[k].chars().count().max(1))
                .collect();
            let total: usize = weights.iter().sum();
            let span = (next_ms - prev_ms) as u64;
            let mut acc = 0usize;
            for (offset, w) in weights.iter().enumerate().skip(1) {
                acc += weights[offset - 1];
                let frac = (acc as u64 * span) / (total as u64);
                let idx = i - 1 + offset;
                if idx <= last_bracketed && start_ms[idx] == 0 {
                    start_ms[idx] = prev_ms + frac as u32;
                }
                let _ = w; // silence unused on the last iteration of the if-branch
            }
        }
        i = j;
    }

    // Enforce strict monotonic + non-zero duration. Push starts forward
    // if they would create a non-monotonic boundary.
    let mut prev = start_ms[0];
    for s in start_ms.iter_mut().skip(1) {
        if *s < prev {
            *s = prev;
        }
        prev = *s;
    }

    // Build aligned lines. End of sub i = start of sub i+1; last sub's
    // end = cluster_end_ms (yt_subs authority).
    let mut out: Vec<AlignedLine> = Vec::with_capacity(n);
    for i in 0..n {
        let start = start_ms[i];
        let end = if i == n - 1 {
            cluster_end_ms
        } else {
            start_ms[i + 1]
        };
        if end <= start {
            warn!(
                idx = i,
                start, end,
                text = %sub_texts[i],
                "yt_subs anchor: zero-duration sub even after fallback"
            );
        }
        out.push(AlignedLine {
            text: sub_texts[i].clone(),
            start_ms: start,
            end_ms: end.max(start.saturating_add(1)),
            words: None,
        });
    }
    out
}

#[cfg(test)]
#[path = "yt_subs_split_tests.rs"]
mod tests;
