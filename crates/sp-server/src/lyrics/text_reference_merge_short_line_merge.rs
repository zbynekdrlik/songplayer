//! Phase 5 short-line merge — collapse consecutive sub-fade-duration lines
//! into one line whose display window comfortably exceeds the Resolume
//! subtitle fade-in (1000 ms, `resolume::handlers::FADE_DURATION_MS`).
//!
//! Why: Phase 3 Claude split + Phase 2 chorus-repeat expansion can emit
//! AlignedLine entries shorter than the 1 s fade. The wall then sees
//! lines replaced mid-fade — text flickers, never fully displays. Saints
//! (id=233) wall-verify 2026-05-11: 22 % of lines were < 1 s and the
//! Bridge cluster (8 adjacent 0.58-0.94 s lines) rendered as a mess.
//!
//! Chorus-repeat sequences (same adjacent text repeated) are preserved
//! so the karaoke renderer can highlight each occurrence separately.

use crate::lyrics::backend::AlignedLine;

/// Merge consecutive AlignedLine entries when the first one is shorter
/// than `min_dur_ms`, the two are truly adjacent (no gap), the combined
/// duration stays under `max_merged_ms`, AND the texts differ (chorus-
/// repeat structure is preserved).
///
/// Operates in-place. Stable and idempotent.
pub(super) fn merge_short_adjacent_lines(
    lines: &mut Vec<AlignedLine>,
    min_dur_ms: u32,
    max_merged_ms: u32,
) {
    if lines.is_empty() {
        return;
    }
    let mut merged: Vec<AlignedLine> = Vec::with_capacity(lines.len());
    merged.push(lines[0].clone());
    for next in lines.iter().skip(1) {
        let cur = merged.last_mut().expect("merged non-empty");
        let cur_dur = cur.end_ms.saturating_sub(cur.start_ms);
        let combined_dur = next.end_ms.saturating_sub(cur.start_ms);
        let adjacent = cur.end_ms == next.start_ms;
        let different_text = cur.text.trim() != next.text.trim();
        if cur_dur < min_dur_ms && adjacent && different_text && combined_dur <= max_merged_ms {
            cur.text = format!("{} {}", cur.text.trim_end(), next.text.trim_start());
            cur.end_ms = next.end_ms;
            // Concat word-level data when both sides have it; drop when
            // either is None (renderer falls back to line-level highlight).
            cur.words = match (cur.words.take(), next.words.as_ref()) {
                (Some(mut cw), Some(nw)) => {
                    cw.extend(nw.iter().cloned());
                    Some(cw)
                }
                _ => None,
            };
        } else {
            merged.push(next.clone());
        }
    }
    *lines = merged;
}
