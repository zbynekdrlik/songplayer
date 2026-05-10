//! yt_subs anchored Claude-split with whisperx internal boundaries.
//!
//! YouTube auto-subtitle (`yt_subs`) lines have line-level timing that
//! is generally accurate, but the line BREAKS reflect the caption-display
//! window — they split mid-phrase ("Thank You for / today That You have
//! made"). For karaoke-quality wall display we re-break long yt_subs
//! lines using Claude (natural-phrase boundaries) without losing the
//! authoritative yt_subs anchor times.
//!
//! Algorithm per yt_subs line:
//! 1. If line text fits the 32-char karaoke cap, keep as-is.
//! 2. Else, Claude split into N sub-lines (`claude_split_lines`,
//!    karaoke-aware prompt; deterministic fallback on parse failure).
//! 3. LCS-align each sub-line's normalized words to the whisperx ASR
//!    words within `[line_start_ms, line_end_ms]`. The earliest matched
//!    whisperx word of `sub[i+1]` becomes the boundary between `sub[i]`
//!    and `sub[i+1]`.
//! 4. Anchor `sub[0].start_ms = line_start_ms` and
//!    `sub[N-1].end_ms = line_end_ms` — yt_subs timing wins at the
//!    line's first and last word.
//!
//! Fallback: if Claude returns one sub OR any sub fails LCS OR a
//! computed boundary is non-monotonic, the line ships unsplit with its
//! original yt_subs timing. We never guess timing — yt_subs timing is
//! authoritative; whisperx fills internal gaps only.

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignedLine, AlignedWord};
use crate::lyrics::text_reference_merge::{
    SUBLINE_MAX_CHARS, claude_split_lines, deterministic_split_one, lcs_align, normalize_word,
};
use std::collections::HashMap;

/// Cluster YouTube caption-window adjacent yt_subs lines back into
/// real phrases. yt_subs auto-captions split mid-phrase wherever the
/// caption-display window ends, producing back-to-back lines with
/// `line[i].end_ms == line[i+1].start_ms`. Real phrase boundaries
/// always have a non-zero gap (singer pauses, instrumental). Merge
/// adjacent (gap == 0) lines so the downstream Claude splitter
/// receives full phrases instead of caption fragments.
///
/// Whitespace inside merged text is normalized to single spaces.
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

/// Re-break a single yt_subs reference line using Claude phrasing +
/// whisperx word boundaries. Returns the original line unsplit on any
/// failure path (Claude error, single-sub split, LCS gap, non-monotonic
/// boundaries). yt_subs anchor times are preserved at the first and
/// last sub.
pub(crate) async fn split_long_line_with_anchors(
    ai_client: &AiClient,
    text: &str,
    line_start_ms: u32,
    line_end_ms: u32,
    asr_words: &[AlignedWord],
) -> Vec<AlignedLine> {
    let unsplit = || {
        vec![AlignedLine {
            text: text.to_string(),
            start_ms: line_start_ms,
            end_ms: line_end_ms,
            words: None,
        }]
    };

    if text.chars().count() <= SUBLINE_MAX_CHARS {
        return unsplit();
    }

    // Claude split (deterministic fallback inside claude_split_lines).
    let lines_in = vec![(0usize, text)];
    let mut split_map: HashMap<usize, Vec<String>> = claude_split_lines(ai_client, &lines_in)
        .await
        .unwrap_or_default();
    let sub_texts = split_map
        .remove(&0)
        .unwrap_or_else(|| deterministic_split_one(text));

    match anchor_subs_to_window(&sub_texts, line_start_ms, line_end_ms, asr_words) {
        Some(out) => out,
        None => unsplit(),
    }
}

/// Anchor pre-split sub-line texts to whisperx word boundaries inside
/// the yt_subs line's `[line_start_ms, line_end_ms]` window. Returns
/// `None` on any LCS-failure or non-monotonic boundary so the caller can
/// fall back to the original unsplit line. yt_subs timing remains
/// authoritative at the first sub's start and the last sub's end.
pub(crate) fn anchor_subs_to_window(
    sub_texts: &[String],
    line_start_ms: u32,
    line_end_ms: u32,
    asr_words: &[AlignedWord],
) -> Option<Vec<AlignedLine>> {
    if sub_texts.len() <= 1 {
        return None;
    }
    let window: Vec<&AlignedWord> = asr_words
        .iter()
        .filter(|w| w.start_ms >= line_start_ms && w.start_ms < line_end_ms)
        .collect();
    if window.is_empty() {
        return None;
    }
    let window_norms: Vec<String> = window.iter().map(|w| normalize_word(&w.text)).collect();
    let window_strs: Vec<&str> = window_norms.iter().map(|s| s.as_str()).collect();

    let mut sub_first_word: Vec<Option<usize>> = Vec::with_capacity(sub_texts.len());
    let mut search_from = 0usize;
    for sub_text in sub_texts {
        let sub_norms: Vec<String> = sub_text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        if sub_norms.is_empty() {
            sub_first_word.push(None);
            continue;
        }
        let sub_strs: Vec<&str> = sub_norms.iter().map(|s| s.as_str()).collect();
        let sub_window = &window_strs[search_from..];
        let alignment = lcs_align(&sub_strs, sub_window);
        let matched: Vec<usize> = alignment.iter().filter_map(|a| *a).collect();
        if matched.is_empty() {
            sub_first_word.push(None);
            continue;
        }
        let first = *matched.first().expect("non-empty");
        let last = *matched.last().expect("non-empty");
        sub_first_word.push(Some(search_from + first));
        search_from = (search_from + last + 1).min(window_strs.len());
    }
    if sub_first_word.iter().any(|x| x.is_none()) {
        return None;
    }
    let sub_first_word: Vec<usize> = sub_first_word.into_iter().map(Option::unwrap).collect();

    let n = sub_texts.len();
    let mut out: Vec<AlignedLine> = Vec::with_capacity(n);
    for i in 0..n {
        let start = if i == 0 {
            line_start_ms
        } else {
            window[sub_first_word[i]].start_ms
        };
        let end = if i == n - 1 {
            line_end_ms
        } else {
            window[sub_first_word[i + 1]].start_ms
        };
        out.push(AlignedLine {
            text: sub_texts[i].clone(),
            start_ms: start,
            end_ms: end,
            words: None,
        });
    }
    for i in 0..n {
        if out[i].end_ms <= out[i].start_ms {
            return None;
        }
        if i > 0 && out[i].start_ms < out[i - 1].end_ms {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "yt_subs_split_tests.rs"]
mod tests;
