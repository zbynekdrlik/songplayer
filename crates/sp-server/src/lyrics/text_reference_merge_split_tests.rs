//! Tests for the Phase-3 splitting functions: `build_split_prompt`,
//! `parse_split_response`, `deterministic_split_lines`. Sibling-included
//! from description_merge.rs to keep the parent files under the
//! 1000-line file-size cap.

#![allow(unused_imports)]

use super::*;

// ── build_split_prompt ────────────────────────────────────────────────────────

#[test]
fn build_split_prompt_includes_input_index_and_text() {
    // Mutation `replace -> String with String::new()` returns "".
    // Mutation `replace -> String with "xyzzy".into()` returns "xyzzy".
    // Both fail when the prompt is required to contain the actual input
    // line text and its index.
    let lines: Vec<(usize, &str)> = vec![
        (3, "A line that is well over thirty-two characters long"),
        (7, "Another long line for splitting"),
    ];
    let prompt = build_split_prompt(&lines);
    assert!(
        prompt.contains("3."),
        "prompt must contain index 3 entry; got {prompt:?}"
    );
    assert!(
        prompt.contains("7."),
        "prompt must contain index 7 entry; got {prompt:?}"
    );
    assert!(
        prompt.contains("A line that is well over"),
        "prompt must contain first line text"
    );
    assert!(
        prompt.contains("Another long line for splitting"),
        "prompt must contain second line text"
    );
    // Body must include the schema line so the model knows what to emit.
    assert!(
        prompt.contains("\"splits\""),
        "prompt must reference schema"
    );
    assert!(prompt.contains("32"), "prompt must reference 32-char cap");
}

#[test]
fn build_split_prompt_handles_empty_input() {
    // No long lines → prompt still has the schema header but no
    // numbered entries. Mutation `String::new()` makes the prompt empty;
    // the schema-marker check below catches it.
    let lines: Vec<(usize, &str)> = vec![];
    let prompt = build_split_prompt(&lines);
    assert!(prompt.contains("\"splits\""), "schema reference required");
    assert!(prompt.contains("32"), "32-char cap reference required");
}

// ── parse_split_response ──────────────────────────────────────────────────────

#[test]
fn parse_split_response_extracts_balanced_object_from_clean_input() {
    let raw = r#"{"splits":[{"i":0,"subs":[{"en":"alpha"},{"en":"beta"}]}]}"#;
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 0);
    assert_eq!(parsed.splits[0].subs.len(), 2);
    assert_eq!(parsed.splits[0].subs[0].en, "alpha");
    assert_eq!(parsed.splits[0].subs[1].en, "beta");
}

#[test]
fn parse_split_response_strips_prose_preamble_and_trailing_text() {
    // Mutation `delete !` on the `if !in_str && b == b'\\'` escape guard
    // would mishandle backslashes inside strings. We don't exercise that
    // boundary here directly but the brace-balance walk must still find
    // a clean object inside surrounding noise.
    let raw = "Here is the result you asked for:\n{\"splits\":[{\"i\":5,\"subs\":[{\"en\":\"x\"}]}]}\n\nDone.";
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 5);
}

#[test]
fn parse_split_response_handles_nested_braces_inside_strings() {
    // A `}` inside a JSON string MUST be ignored by the brace walker.
    // Mutation `delete !` on the in_str / b'"' branches would either
    // misclassify the string boundary or count the `}` as a brace.
    let raw = r#"{"splits":[{"i":1,"subs":[{"en":"a } b"}]}]}"#;
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits[0].subs[0].en, "a } b");
}

#[test]
fn parse_split_response_handles_escaped_quote_in_string() {
    // Escaped `\"` followed by `}}` — without the `esc` tracking, the
    // walker would prematurely flip in_str=false at the `\"`, count
    // both `}}` as real, and truncate the slice early.
    let raw = r#"{"splits":[{"i":2,"subs":[{"en":"x\"}}"}]}]}"#;
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits[0].subs[0].en, "x\"}}");
}

#[test]
fn parse_split_response_returns_err_when_no_balanced_object() {
    // Mutations that bypass the start/end matching (e.g. `delete !` on
    // some guard so depth never reaches 0) would still err here because
    // the input has no `{` at all.
    let raw = "no json here at all";
    let result = parse_split_response(raw);
    assert!(result.is_err());
}

// ── deterministic_split_lines ────────────────────────────────────────────────

#[test]
fn deterministic_split_lines_returns_one_entry_per_input() {
    // Mutation `replace -> HashMap::new()` returns empty. Mutation
    // `from_iter([(0, vec![])])` returns the wrong content. Test asserts
    // every input index is present with non-empty subs.
    let inputs: Vec<(usize, &str)> = vec![
        (0, "A thousand generations falling down in worship"),
        (4, "Your name stands above them all"),
    ];
    let result = deterministic_split_lines(&inputs);
    assert_eq!(result.len(), 2, "one entry per input index");
    assert!(result.contains_key(&0), "must include index 0");
    assert!(result.contains_key(&4), "must include index 4");
    let subs0 = result.get(&0).unwrap();
    assert!(!subs0.is_empty(), "subs for 0 must be non-empty");
    // Each sub must respect the 32-char cap.
    for s in subs0 {
        assert!(
            s.chars().count() <= SUBLINE_MAX_CHARS,
            "sub over cap: {s:?}"
        );
    }
    let subs4 = result.get(&4).unwrap();
    assert!(!subs4.is_empty(), "subs for 4 must be non-empty");
    for s in subs4 {
        assert!(s.chars().count() <= SUBLINE_MAX_CHARS);
    }
}

#[test]
fn deterministic_split_lines_preserves_input_indices() {
    // Mutation `from_iter([(1, ...)])` shifts the index by one. Use
    // non-zero non-one indices so both the (0, ..) and (1, ..) constant
    // mutations are distinguishable from the original.
    let inputs: Vec<(usize, &str)> = vec![(7, "Longer line that needs splitting")];
    let result = deterministic_split_lines(&inputs);
    assert_eq!(result.len(), 1);
    assert!(
        result.contains_key(&7),
        "key must be 7 (not 0 or 1 — those are the constant-mutation values)"
    );
    let subs = result.get(&7).unwrap();
    assert!(!subs.is_empty());
}
