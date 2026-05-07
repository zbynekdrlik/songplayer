//! Phase 1.5: align Claude-added reference lines against the unmatched
//! ASR audio window each one belongs to. Runs after Phase 1's word→line
//! mapping and after the reference list has been EXPANDED to include the
//! added lines. Phases 2 / 2.5 / 2.6 / 2.7 / 3 / 4 / 5 then run unchanged
//! over the expanded reference + the union of Phase 1 + Phase 1.5 emits.

use super::{AsrWord, LineEmit, lcs_align, normalize_word};

pub(crate) fn align_added_lines(
    expanded_ref_lines: &[String],
    added_expanded_indices: &[usize],
    asr_words: &[AsrWord],
    existing_emits: &[LineEmit],
) -> Vec<LineEmit> {
    // Sorted (min_asr_idx, max_asr_idx) for every existing emit.
    let mut occupied: Vec<(usize, usize)> = existing_emits
        .iter()
        .filter_map(|e| {
            let mn = *e.asr_word_indices.iter().min()?;
            let mx = *e.asr_word_indices.iter().max()?;
            Some((mn, mx))
        })
        .collect();
    occupied.sort_unstable();

    let mut out: Vec<LineEmit> = Vec::with_capacity(added_expanded_indices.len());
    let mut prev_added_end: Option<usize> = None;

    for &expanded_idx in added_expanded_indices.iter() {
        let line_text = expanded_ref_lines[expanded_idx].clone();

        // Lower bound = first ASR idx after both the latest already-added
        // emit's max and the latest occupied span at-or-before this slot.
        let lo = prev_added_end.map(|e| e + 1).unwrap_or(0);

        // Upper bound = first occupied min that's strictly greater than lo.
        let hi = occupied
            .iter()
            .find(|(mn, _)| *mn > lo)
            .map(|(mn, _)| *mn)
            .unwrap_or(asr_words.len());

        if lo >= hi {
            out.push(LineEmit {
                text: line_text,
                asr_word_indices: Vec::new(),
            });
            continue;
        }

        let window_norms: Vec<&str> = (lo..hi).map(|i| asr_words[i].norm.as_str()).collect();
        let line_norm_owned: Vec<String> = line_text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        let line_norms: Vec<&str> = line_norm_owned.iter().map(|s| s.as_str()).collect();

        let alignment = lcs_align(&line_norms, &window_norms);
        let matched_in_window: Vec<usize> =
            alignment.iter().filter_map(|a| a.map(|j| lo + j)).collect();

        if let Some(&mx) = matched_in_window.iter().max() {
            prev_added_end = Some(mx);
        }

        out.push(LineEmit {
            text: line_text,
            asr_word_indices: matched_in_window,
        });
    }
    out
}

#[cfg(test)]
#[path = "text_reference_merge_added_tests.rs"]
mod tests;
