//! Phase 1.5: align Claude-added reference lines against the unmatched
//! ASR audio window each one belongs to. Each added line carries an
//! `after_line` index — the original description line it semantically
//! follows. The audio window for an added line spans from the END of
//! that description line's matched ASR audio to the START of the next
//! non-empty description emit.

use std::collections::HashMap;

use super::mapping::AddedRefLine;
use super::{AsrWord, LineEmit, lcs_align, normalize_word};

pub(crate) fn align_added_lines(
    expanded_ref_lines: &[String],
    added: &[AddedRefLine],
    expanded_indices: &[usize],
    orig_to_expanded: &[usize],
    asr_words: &[AsrWord],
    existing_emits: &[LineEmit],
) -> Vec<LineEmit> {
    let mut out: Vec<LineEmit> = Vec::with_capacity(added.len());
    let mut consumed_at: HashMap<usize, usize> = HashMap::new();

    for (i, a) in added.iter().enumerate() {
        let line_text = expanded_ref_lines[expanded_indices[i]].clone();

        // Anchor: the description line at `after_line` (in original idx).
        let after_expanded = match orig_to_expanded.get(a.after_line) {
            Some(&v) => v,
            None => {
                out.push(LineEmit {
                    text: line_text,
                    asr_word_indices: Vec::new(),
                });
                continue;
            }
        };

        // Lower bound = max ASR idx consumed by the description line's emit
        // (or by a prior added line at the same after_line). +1 to start AFTER.
        let prior_max = match consumed_at.get(&a.after_line) {
            Some(&m) => m,
            None => existing_emits
                .get(after_expanded)
                .and_then(|e| e.asr_word_indices.iter().max().copied())
                .unwrap_or(0),
        };
        let lo = prior_max.saturating_add(1);

        // Upper bound = first non-empty emit AFTER the anchor's slot. Scan
        // forward through expanded indices; take its min ASR idx.
        let hi = existing_emits
            .iter()
            .skip(after_expanded + 1)
            .filter_map(|e| e.asr_word_indices.iter().min().copied())
            .next()
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
        let matched: Vec<usize> = alignment.iter().filter_map(|a| a.map(|j| lo + j)).collect();

        if let Some(&mx) = matched.iter().max() {
            consumed_at.insert(a.after_line, mx);
        }

        out.push(LineEmit {
            text: line_text,
            asr_word_indices: matched,
        });
    }
    out
}

#[cfg(test)]
#[path = "text_reference_merge_added_tests.rs"]
mod tests;
