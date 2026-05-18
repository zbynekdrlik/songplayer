//! Tests for `is_allowed_text_source`. Sibling file referenced by
//! `orchestrator.rs` under
//! `#[path = "orchestrator_gate_tests.rs"] #[cfg(test)] mod is_allowed_text_source_tests;`
//! to keep `orchestrator.rs` under the 1000-line airuleset cap.

use super::is_allowed_text_source;
use crate::lyrics::provider::CandidateText;

fn candidate(source: &str, has_timing: bool, lines: Vec<&str>) -> CandidateText {
    CandidateText {
        source: source.to_string(),
        lines: lines.into_iter().map(String::from).collect(),
        has_timing,
        line_timings: None,
    }
}

#[test]
fn empty_candidates_rejected() {
    assert!(!is_allowed_text_source(&[]));
}

#[test]
fn yt_subs_with_timing_accepted() {
    assert!(is_allowed_text_source(&[candidate(
        "yt_subs",
        true,
        vec!["line"]
    )]));
}

#[test]
fn yt_subs_without_timing_rejected() {
    assert!(!is_allowed_text_source(&[candidate(
        "yt_subs",
        false,
        vec!["line"]
    )]));
}

#[test]
fn lrclib_with_timing_accepted() {
    assert!(is_allowed_text_source(&[candidate(
        "lrclib",
        true,
        vec!["line"]
    )]));
}

#[test]
fn lrclib_without_timing_rejected() {
    assert!(!is_allowed_text_source(&[candidate(
        "lrclib",
        false,
        vec!["line"]
    )]));
}

#[test]
fn spotify_with_timing_accepted() {
    assert!(is_allowed_text_source(&[candidate(
        "spotify",
        true,
        vec!["line"]
    )]));
}

#[test]
fn spotify_without_timing_rejected() {
    assert!(!is_allowed_text_source(&[candidate(
        "spotify",
        false,
        vec!["line"]
    )]));
}

#[test]
fn description_with_lines_accepted() {
    assert!(is_allowed_text_source(&[candidate(
        "description",
        false,
        vec!["a", "b"]
    )]));
}

#[test]
fn description_empty_lines_rejected() {
    assert!(!is_allowed_text_source(&[candidate(
        "description",
        false,
        vec![]
    )]));
}

#[test]
fn genius_always_rejected() {
    assert!(!is_allowed_text_source(&[candidate(
        "genius",
        true,
        vec!["line"]
    )]));
    assert!(!is_allowed_text_source(&[candidate(
        "genius",
        false,
        vec!["line"]
    )]));
}

#[test]
fn mixed_genius_plus_yt_subs_with_timing_accepted() {
    assert!(is_allowed_text_source(&[
        candidate("genius", true, vec!["line"]),
        candidate("yt_subs", true, vec!["line"]),
    ]));
}
