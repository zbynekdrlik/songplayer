//! Tests for `merge_short_adjacent_lines` — Phase 5 short-line consolidation.
//!
//! Sub-1s lines flicker on the Resolume wall because subtitle fade-in takes
//! 1000 ms; this pass merges consecutive too-short adjacent entries with
//! different text into one >=1s line. Chorus-repeat sequences (identical
//! adjacent text) are preserved so the karaoke renderer can highlight each
//! occurrence separately.

use crate::lyrics::backend::AlignedLine;

use super::merge_short_adjacent_lines;

fn line(text: &str, start_ms: u32, end_ms: u32) -> AlignedLine {
    AlignedLine {
        text: text.to_string(),
        start_ms,
        end_ms,
        words: None,
    }
}

#[test]
fn merges_two_short_adjacent_different_text_lines() {
    let mut lines = vec![
        line("I died to myself", 36640, 37420),   // 780 ms
        line("and He lives in me", 37420, 39400), // 1980 ms (already long)
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // First line is short AND adjacent AND text differs → merge.
    assert_eq!(lines.len(), 1, "should merge into 1 line: {lines:?}");
    assert_eq!(lines[0].text, "I died to myself and He lives in me");
    assert_eq!(lines[0].start_ms, 36640);
    assert_eq!(lines[0].end_ms, 39400);
}

#[test]
fn preserves_chorus_repeats_identical_text() {
    let mut lines = vec![
        line("It's the power of Jesus", 130000, 131000), // 1000 ms (>= min) but adjacent
        line("It's the power of Jesus", 131000, 132000), // identical text
        line("It's the power of Jesus", 132000, 133000),
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // Lines >= 1000ms also don't merge by short-line trigger. Identical text
    // makes the merge guard fire even if first was short. Verify all 3 kept.
    assert_eq!(
        lines.len(),
        3,
        "chorus repeats must stay separate: {lines:?}"
    );
}

#[test]
fn does_not_merge_when_gap_present() {
    let mut lines = vec![
        line("Short line A", 10000, 10500), // 500 ms
        line("Next line B", 11000, 12500),  // gap of 500 ms before
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // Not truly adjacent (gap = 500 ms) → no merge.
    assert_eq!(
        lines.len(),
        2,
        "non-adjacent lines must NOT merge: {lines:?}"
    );
}

#[test]
fn caps_merged_duration() {
    let mut lines = vec![
        line("A", 0, 900),     // 900 ms
        line("B", 900, 1800),  // 900 ms
        line("C", 1800, 2700), // 900 ms
        line("D", 2700, 3600), // 900 ms
        line("E", 3600, 4500), // 900 ms — combined with all prior = 4500 ms > 4000 cap
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // First merge: A+B = 0..1800 = 1800 ms. Now cur dur >= min — stop merging.
    // Result depends on iteration order; the critical invariant is no merged
    // entry exceeds the cap.
    for l in &lines {
        let dur = l.end_ms - l.start_ms;
        assert!(
            dur <= 4000,
            "merged line must not exceed cap (got dur={dur}): {l:?}"
        );
    }
}

#[test]
fn merges_chain_of_short_adjacent_lines() {
    let mut lines = vec![
        line("Go 'head testify", 107000, 107580), // 580 ms
        line("Go 'head and tell 'em your story", 107580, 108160), // 580 ms
        line("As we begin to witness", 108160, 108840), // 680 ms
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // First two merge into 580+580=1160ms line (>= min so chain stops there).
    // Third stays separate.
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0].text,
        "Go 'head testify Go 'head and tell 'em your story"
    );
    assert_eq!(lines[0].start_ms, 107000);
    assert_eq!(lines[0].end_ms, 108160);
    assert_eq!(lines[1].text, "As we begin to witness");
}

#[test]
fn empty_input_is_no_op() {
    let mut lines: Vec<AlignedLine> = vec![];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    assert!(lines.is_empty());
}

#[test]
fn single_line_input_unchanged() {
    let mut lines = vec![line("only", 0, 500)];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "only");
}

#[test]
fn does_not_merge_when_first_already_long() {
    let mut lines = vec![
        line("This line is long", 0, 1500), // 1500 ms (>= min)
        line("Next line", 1500, 1800),      // 300 ms (short but follows long)
    ];
    merge_short_adjacent_lines(&mut lines, 1000, 4000);
    // First is NOT short → merge guard doesn't fire on first iteration.
    // Result has both lines.
    assert_eq!(lines.len(), 2);
}
