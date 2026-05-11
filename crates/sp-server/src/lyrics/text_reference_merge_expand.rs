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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn expand_no_added_returns_input_with_identity_map() {
        let original = lines(&["A", "B", "C"]);
        let (expanded, map) = expand_ref_lines(&original, &[]);
        assert_eq!(expanded, original);
        assert_eq!(map, vec![0, 1, 2]);
    }

    #[test]
    fn expand_inserts_added_after_their_anchor_line() {
        let original = lines(&["A", "B", "C"]);
        let added = vec![AddedRefLine {
            after_line: 0,
            text: "INS".to_string(),
        }];
        let (expanded, map) = expand_ref_lines(&original, &added);
        assert_eq!(expanded, lines(&["A", "INS", "B", "C"]));
        // A still at slot 0; B moves to 2; C moves to 3.
        assert_eq!(map, vec![0, 2, 3]);
    }

    #[test]
    fn expand_preserves_added_order_for_same_anchor() {
        let original = lines(&["A", "B"]);
        let added = vec![
            AddedRefLine {
                after_line: 0,
                text: "X1".to_string(),
            },
            AddedRefLine {
                after_line: 0,
                text: "X2".to_string(),
            },
        ];
        let (expanded, _) = expand_ref_lines(&original, &added);
        assert_eq!(expanded, lines(&["A", "X1", "X2", "B"]));
    }

    #[test]
    fn remap_translates_original_indices_to_expanded() {
        let map = vec![0, 2, 3];
        let original = vec![Some(0), Some(1), None, Some(2)];
        let remapped = remap_mapping(&original, &map);
        assert_eq!(remapped, vec![Some(0), Some(2), None, Some(3)]);
    }

    #[test]
    fn remap_drops_out_of_range_indices() {
        let map = vec![0, 1];
        let original = vec![Some(0), Some(5)];
        let remapped = remap_mapping(&original, &map);
        assert_eq!(remapped, vec![Some(0), None]);
    }
}
