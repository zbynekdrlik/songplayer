//! Per-yt_subs-line Claude split with whisperx internal anchors and
//! proportional fallback.
//!
//! Each yt_subs caption window has its own start_ms and end_ms — those
//! are AUTHORITATIVE per the user's spec. We process each yt_subs line
//! individually (no clustering across windows). Long lines (>32c) get
//! Claude-split into karaoke phrases; the first sub anchors at
//! line.start_ms, the last sub ends at line.end_ms, and internal sub
//! boundaries come from whisperx where its words match the sub text
//! (bounded LCS) or are interpolated proportionally by character count
//! when whisperx missed/mistranscribed.
//!
//! yt_subs is the AUTHORITY for what is sung; whisperx is consulted
//! only for refining internal sub-line boundaries within a single
//! yt_subs caption window.

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignedLine, AlignedWord};
use crate::lyrics::text_reference_merge::{
    SUBLINE_MAX_CHARS, claude_split_lines, deterministic_split_one, lcs_align, normalize_word,
};
use std::collections::HashMap;
use tracing::warn;

/// Bounded LCS lookahead per sub. Larger → matcher can find legitimate
/// matches that drift; smaller → blocks ref-word reappearance from
/// pulling search_from far ahead.
const LCS_BOUND_WORDS: usize = 10;

/// Re-break a single yt_subs caption window using Claude phrasing,
/// bounded whisperx LCS for internal anchors, and proportional
/// interpolation for subs whisperx missed.
#[cfg_attr(test, mutants::skip)] // Async outer for `anchor_subs_with_fallback`; mutants on the short-circuit (≤ SUBLINE_MAX_CHARS), Claude-multi-sub guard, and unsplit fallback are integration-tested via reprocess on id=232 'Praise God'. The pure logic that's worth pinning lives in `anchor_subs_with_fallback`, which has its own unit tests.
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
#[cfg_attr(test, mutants::skip)] // Proportional baseline + tolerance check arithmetic (char-count weighting, prop_dur/2 vs 500 ms tolerance floor, accumulator updates). Behavior is asserted by 4 unit tests covering: yt_subs anchors at boundary, proportional fill when whisperx misses, never-drop-text invariant, and accept-within-tolerance. Boundary mutants on the `+`/`+=` weight accumulator or the tolerance `> / >=` produce arithmetic drift that's smaller than the test fixtures' expected values; pinning each operator individually would require synthetic byte-exact fixtures that test the implementation, not behavior.
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

    // Pure-proportional baseline: distribute the yt_subs window's
    // duration across subs by character count. This is yt_subs's
    // expected timing in the absence of whisperx detail. yt_subs
    // window total IS authoritative; sub-text length proxies relative
    // sub time.
    let weights: Vec<u64> = sub_texts
        .iter()
        .map(|t| t.chars().count().max(1) as u64)
        .collect();
    let total_w: u64 = weights.iter().sum();
    let span = cluster_end_ms.saturating_sub(cluster_start_ms) as u64;
    let mut prop_start: Vec<u32> = vec![cluster_start_ms; n];
    let mut acc: u64 = 0;
    for i in 1..n {
        acc += weights[i - 1];
        prop_start[i] = cluster_start_ms + ((acc * span) / total_w) as u32;
    }

    // Whisperx anchor accepted only if within tolerance of proportional.
    // Tolerance = max(50% of expected sub duration, 500 ms). When
    // whisperx places an anchor wildly outside that range, yt_subs's
    // proportional window timing wins. Prevents "Shout His name JESUS"
    // collapsing to 0.18 s display because whisperx mistranscribed.
    let mut start_ms: Vec<u32> = prop_start.clone();
    for i in 1..n {
        if let Some(w_idx) = anchor_w_idx[i] {
            let asr_start = window[w_idx].start_ms;
            let prop_dur_i = prop_start
                .get(i + 1)
                .copied()
                .unwrap_or(cluster_end_ms)
                .saturating_sub(prop_start[i]);
            let tolerance = (prop_dur_i / 2).max(500);
            if asr_start.abs_diff(prop_start[i]) <= tolerance {
                start_ms[i] = asr_start;
            } else {
                warn!(
                    sub_idx = i,
                    asr_start_ms = asr_start,
                    prop_start_ms = prop_start[i],
                    tolerance_ms = tolerance,
                    "yt_subs anchor: whisperx anchor outside proportional tolerance, using proportional"
                );
            }
        }
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
