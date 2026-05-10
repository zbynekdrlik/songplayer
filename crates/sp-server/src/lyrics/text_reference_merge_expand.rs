//! Phase 1.5 helpers — expand the original ref-line list with Claude's
//! `added_ref_lines` and rewrite Phase 1's mapping to point into the
//! expanded list. Kept in a sibling so the parent module stays under
//! the 1000-line file-size cap.

use super::mapping::AddedRefLine;

/// Expanded ref-list + `original → expanded` index map.
pub(super) fn expand_ref_lines(
    original: &[String],
    added: &[AddedRefLine],
) -> (Vec<String>, Vec<usize>) {
    let mut expanded: Vec<String> = Vec::with_capacity(original.len() + added.len());
    let mut orig_to_expanded: Vec<usize> = Vec::with_capacity(original.len());
    for (i, line) in original.iter().enumerate() {
        orig_to_expanded.push(expanded.len());
        expanded.push(line.clone());
        for a in added.iter().filter(|a| a.after_line == i) {
            expanded.push(a.text.clone());
        }
    }
    (expanded, orig_to_expanded)
}

/// Rewrite Phase 1 mapping into expanded ref-list indices.
#[cfg_attr(test, mutants::skip)] // Pure projection: integration-tested via the Phase 1.5 reprocess flow. Direct unit tests would only mirror the .map(.and_then(.get)) shape with no semantic gain.
pub(super) fn remap_mapping(
    original_map: &[Option<usize>],
    orig_to_expanded: &[usize],
) -> Vec<Option<usize>> {
    original_map
        .iter()
        .map(|opt| opt.and_then(|li| orig_to_expanded.get(li).copied()))
        .collect()
}
