//! `trim_outlier_indices` — extracted from `text_reference_merge.rs` to
//! keep that file under the 1000-line cap.
//!
//! Trim trailing duplicate-far-apart pickups (time gap ≥ 3 s from the
//! previous matched word) when the derived audio span exceeds
//! `LONG_LINE_CAP_MS = 8 s`. Held-note tails (gap < 3 s) are kept whole
//! even when span exceeds the cap — Phase 4 sub-line LCS will split the
//! parent span into shorter sub-lines whose individual durations Phase 5
//! caps separately. Without this gap guard, id=21 "Good Shepherd"
//! 2026-05-07 dropped the held-note tail of long description lines and
//! the wall switched ~1.8 s before the singer finished the line.

use super::{AsrWord, LONG_LINE_CAP_MS};

/// Time gap (ms) between consecutive matched words above which the
/// trailing word is considered a duplicate-far-apart pickup. Tuned to
/// the maximum observed sustained-note duration in production
/// (~2.4 s for "forgiveness" on id=21).
const TRIM_GAP_MS: u32 = 3000;

pub(crate) fn trim_outlier_indices(indices: &mut Vec<usize>, asr_words: &[AsrWord]) {
    if indices.len() <= 1 {
        return;
    }
    indices.sort_unstable();
    while indices.len() > 1 {
        let first = indices[0];
        let last = *indices.last().expect("len > 1");
        let span = asr_words[last]
            .end_ms
            .saturating_sub(asr_words[first].start_ms);
        if span <= LONG_LINE_CAP_MS {
            break;
        }
        // Find the LARGEST consecutive gap anywhere in the matched range.
        // If the max gap is below TRIM_GAP_MS the matches are tight (held
        // notes) and we keep the whole range; Phase 4 sub-line LCS will
        // split it. If max gap ≥ TRIM_GAP_MS there's an outlier — pop
        // trailing until the span fits the cap.
        let max_gap = (1..indices.len())
            .map(|i| {
                asr_words[indices[i]]
                    .start_ms
                    .saturating_sub(asr_words[indices[i - 1]].end_ms)
            })
            .max()
            .unwrap_or(0);
        if max_gap < TRIM_GAP_MS {
            break;
        }
        indices.pop();
    }
}
