//! Tests for `merge_deterministic` (issue #78 regression coverage).
//!
//! Split from `claude_merge_tests.rs` to keep both files under the airuleset
//! 1000-line cap. Sibling include declared in `claude_merge.rs` alongside
//! the original tests module.
//!
//! Description and override sources arrive as clean text without timing.
//! The pre-fix path passed them through claude_merge which iterated WhisperX
//! PHRASES (audio-gap-split) and produced one output line per phrase. For a
//! 6-min worship song with 60 phrases this destroyed the description's clean
//! 25-line natural segmentation and shipped 95 fragmented lines, unusable on
//! the LED wall (issue #78). The deterministic mapper preserves the
//! reference's exact line count by construction.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::tier1::CandidateText;

// ── Local helpers (mirror claude_merge_tests.rs to keep tests independent) ──

fn make_word(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.to_string(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn make_asr_with_words(lines: &[(&str, u32, u32, Vec<AlignedWord>)]) -> AlignedTrack {
    AlignedTrack {
        lines: lines
            .iter()
            .map(|(text, s, e, words)| AlignedLine {
                text: text.to_string(),
                start_ms: *s,
                end_ms: *e,
                words: Some(words.clone()),
            })
            .collect(),
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    }
}

fn cand(source: &str, lines: &[&str]) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: lines.iter().map(|s| (*s).into()).collect(),
        line_timings: None,
        has_timing: false,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn merge_deterministic_preserves_reference_line_count() {
    // 3 reference lines, ASR with 14 words simulating WhisperX over-split.
    // Output must have exactly 3 lines, not one-per-phrase.
    let ref_lines: Vec<String> = vec![
        "Holy is the Lamb of God".into(),
        "Worthy is the King".into(),
        "Forever we will sing".into(),
    ];
    let asr = make_asr_with_words(&[(
        "holy is the lamb of god worthy is the king forever we will sing",
        0,
        12000,
        vec![
            make_word("holy", 0, 500),
            make_word("is", 600, 800),
            make_word("the", 900, 1100),
            make_word("lamb", 1200, 1700),
            make_word("of", 1800, 2000),
            make_word("god", 2100, 2700),
            make_word("worthy", 4000, 4600),
            make_word("is", 4700, 4900),
            make_word("the", 5000, 5200),
            make_word("king", 5300, 5900),
            make_word("forever", 8000, 8800),
            make_word("we", 8900, 9100),
            make_word("will", 9200, 9500),
            make_word("sing", 9600, 10500),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.lines.len(),
        3,
        "deterministic mapper must preserve reference line count (got {})",
        result.lines.len()
    );
}

#[test]
fn merge_deterministic_uses_reference_text_verbatim() {
    let ref_lines: Vec<String> = vec!["Line one".into(), "Line two".into()];
    let asr = make_asr_with_words(&[(
        "line one line two",
        0,
        4000,
        vec![
            make_word("line", 0, 500),
            make_word("one", 600, 1000),
            make_word("line", 2000, 2500),
            make_word("two", 2600, 3000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(result.lines[0].text, "Line one");
    assert_eq!(result.lines[1].text, "Line two");
}

#[test]
fn merge_deterministic_provenance_includes_source() {
    let ref_lines: Vec<String> = vec!["Hello world".into()];
    let asr = make_asr_with_words(&[(
        "hello world",
        0,
        2000,
        vec![make_word("hello", 0, 500), make_word("world", 600, 1000)],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.provenance, "description+whisperx-large-v3@rev1",
        "provenance must show description as the text-source authority, no +claude-merge suffix"
    );
}

#[test]
fn merge_deterministic_words_field_is_none_per_line_timing_only_rule() {
    // Per feedback_line_timing_only.md: line-level only, no per-word output.
    let ref_lines: Vec<String> = vec!["A B".into(), "C D".into()];
    let asr = make_asr_with_words(&[(
        "a b c d",
        0,
        4000,
        vec![
            make_word("a", 0, 500),
            make_word("b", 600, 1000),
            make_word("c", 2000, 2500),
            make_word("d", 2600, 3000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "override");
    for l in &result.lines {
        assert!(
            l.words.is_none(),
            "merge_deterministic must emit words: None per feedback_line_timing_only.md"
        );
    }
}

#[test]
fn merge_deterministic_timings_strictly_increasing() {
    // Output line[i].start_ms must be >= line[i-1].end_ms; non-decreasing
    // start, positive duration. Per feedback_line_timing_only sanitizer
    // rules and the v9-v10 history.
    let ref_lines: Vec<String> = vec![
        "First line".into(),
        "Second line".into(),
        "Third line".into(),
    ];
    let asr = make_asr_with_words(&[(
        "first line second line third line",
        0,
        9000,
        vec![
            make_word("first", 0, 500),
            make_word("line", 600, 1000),
            make_word("second", 3000, 3500),
            make_word("line", 3600, 4000),
            make_word("third", 6000, 6500),
            make_word("line", 6600, 7000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    let mut prev_start = 0u32;
    let mut prev_end = 0u32;
    for (i, l) in result.lines.iter().enumerate() {
        assert!(
            l.start_ms >= prev_end,
            "line[{i}].start_ms ({}) must be >= prev line.end_ms ({prev_end})",
            l.start_ms
        );
        assert!(
            l.start_ms > prev_start || i == 0,
            "line[{i}].start_ms must be strictly > prev line.start_ms"
        );
        assert!(l.end_ms > l.start_ms, "line[{i}] zero-duration");
        prev_start = l.start_ms;
        prev_end = l.end_ms;
    }
}

#[test]
fn merge_deterministic_handles_unmatched_reference_lines() {
    // Reference has 3 lines, but ASR audio only matches words for line 0 and 2.
    // Line 1 has no audio support — mapper must still emit it with a placeholder
    // timing in chronological order, not skip it. Output count == 3.
    let ref_lines: Vec<String> = vec![
        "Match one".into(),
        "Reference only no audio".into(),
        "Match three".into(),
    ];
    let asr = make_asr_with_words(&[(
        "match one match three",
        0,
        6000,
        vec![
            make_word("match", 0, 500),
            make_word("one", 600, 1000),
            make_word("match", 4000, 4500),
            make_word("three", 4600, 5000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.lines.len(),
        3,
        "must emit all reference lines including unmatched ones"
    );
    assert_eq!(result.lines[1].text, "Reference only no audio");
    // Unmatched line falls between the two matched ones chronologically.
    assert!(result.lines[1].start_ms >= result.lines[0].end_ms);
    assert!(result.lines[1].end_ms <= result.lines[2].start_ms);
}

#[test]
fn merge_branches_to_deterministic_for_description_source() {
    // When the best authoritative source is "description", merge() (the
    // public entry) dispatches to merge_deterministic and produces output
    // with line count == reference line count and no +claude-merge suffix
    // in provenance.
    let only_desc = vec![cand("description", &["Ref line A", "Ref line B"])];
    let best = best_authoritative_candidate(&only_desc).expect("description present");
    assert_eq!(best.source, "description");

    let asr = make_asr_with_words(&[(
        "ref line a ref line b",
        0,
        4000,
        vec![
            make_word("ref", 0, 200),
            make_word("line", 300, 600),
            make_word("a", 700, 1000),
            make_word("ref", 2000, 2200),
            make_word("line", 2300, 2600),
            make_word("b", 2700, 3000),
        ],
    )]);
    let result = merge_deterministic(&asr, &best.lines, &best.source);
    assert_eq!(result.lines.len(), 2);
    assert!(!result.provenance.contains("+claude-merge"));
    assert!(result.provenance.starts_with("description+"));
}
