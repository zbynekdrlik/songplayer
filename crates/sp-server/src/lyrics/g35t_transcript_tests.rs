//! Tests for the g35t base-tier line grouping (#159). Salvaged from the
//! deleted `asr_path/fallback.rs` + `sanitize.rs` suites, re-typed for
//! `g35t_client::AsrWord`, plus a group-boundary case set that pins the
//! gap/coalesce/sanitize invariants.

use super::*;
use crate::lyrics::g35t_client::AsrWord;

fn w(text: &str, start: u64, end: u64) -> AsrWord {
    AsrWord {
        text: text.to_string(),
        start_ms: start,
        end_ms: end,
    }
}

#[test]
fn empty_input_yields_empty_output() {
    assert!(words_to_lines(&[]).is_empty());
}

#[test]
fn blank_words_are_skipped() {
    // Whitespace-only tokens must not seed a line.
    let words = vec![w("  ", 0, 100), w("hello", 200, 700), w("world", 800, 1300)];
    let lines = words_to_lines(&words);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].en, "hello world");
}

// --- raw group_on_silence: pin the gap boundary before coalesce masks it ---

#[test]
fn group_exact_gap_does_not_split() {
    // gap == LINE_GAP_MS (400) must NOT split — only strictly greater does.
    let words = vec![w("a", 0, 500), w("b", 900, 1400)]; // gap exactly 400
    assert_eq!(group_on_silence(&words).len(), 1);
}

#[test]
fn group_just_over_gap_splits() {
    let words = vec![w("a", 0, 500), w("b", 901, 1400)]; // gap 401 > 400
    assert_eq!(group_on_silence(&words).len(), 2);
}

#[test]
fn group_small_gaps_stay_one_group() {
    let words = vec![w("a", 0, 300), w("b", 350, 600), w("c", 650, 900)];
    let groups = group_on_silence(&words);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].len(), 3);
}

#[test]
fn no_silence_gap_yields_single_line() {
    let words = vec![
        w("hello", 0, 500),
        w("world", 600, 1100),
        w("again", 1150, 1600),
    ];
    let lines = words_to_lines(&words);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].en, "hello world again");
    assert!(lines[0].words.is_none());
}

#[test]
fn large_gap_starts_new_line() {
    let words = vec![
        w("the", 0, 300),
        w("greatest", 400, 900),
        w("name", 1000, 1500),
        w("we", 3500, 3800),
        w("praise", 3900, 4400),
        w("him", 4500, 5000),
    ];
    let lines = words_to_lines(&words);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].en, "the greatest name");
    assert_eq!(lines[1].en, "we praise him");
}

#[test]
fn short_fragment_coalesces_into_neighbour() {
    let words = vec![
        w("the", 0, 300),
        w("greatest", 400, 900),
        w("name", 1000, 1500),
        w("I", 2100, 2300),    // 1-word group
        w("know", 2900, 3300), // 1-word group
    ];
    let lines = words_to_lines(&words);
    assert!(
        lines.iter().all(|l| l.en.split_whitespace().count() >= 2),
        "no 1-word lines: {lines:?}"
    );
}

#[test]
fn does_not_coalesce_across_long_gap() {
    let words = vec![
        w("yeah", 0, 400),
        w("we", 6000, 6300),
        w("praise", 6400, 6900),
        w("him", 7000, 7500),
    ];
    let lines = words_to_lines(&words);
    // gap 5600 > MAX_MERGE_GAP_MS (1500) → "yeah" stays its own line.
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].en, "yeah");
}

#[test]
fn output_lines_always_have_words_none() {
    let words = vec![w("a", 0, 500), w("b", 600, 1100), w("c", 1150, 1600)];
    assert!(words_to_lines(&words).iter().all(|l| l.words.is_none()));
}

// --- sanitize invariants (via the public path) ---

#[test]
fn sanitize_clamps_overlap_and_min_duration() {
    // Two overlapping short lines separated only by a tiny gap so they stay
    // ONE group is not what we want here; use a real gap to force two lines
    // then assert monotonic/no-overlap/min-duration on the output.
    let words = vec![
        w("a", 0, 50), // 50ms line → clamped to >=200ms
        w("b", 500, 520),
        w("c", 560, 600), // second group, starts 500
    ];
    let lines = words_to_lines(&words);
    // strictly non-overlapping, each >= MIN_LINE_DURATION_MS, monotonic starts
    let mut prev_end = 0u64;
    for l in &lines {
        assert!(l.start_ms >= prev_end, "monotonic start: {lines:?}");
        assert!(
            l.end_ms >= l.start_ms + MIN_LINE_DURATION_MS,
            "min dur: {lines:?}"
        );
        prev_end = l.end_ms;
    }
}

#[test]
fn source_label_is_gemini_transcribe() {
    assert_eq!(SOURCE_G35T, "gemini-3-5-transcribe");
}

// --- sanitize_lines: direct invariant tests (salvaged from asr_path/sanitize.rs,
// pinning the monotonic-floor / min-duration clamps against mutation) ---

fn line(start: u64, end: u64) -> sp_core::lyrics::LyricsLine {
    sp_core::lyrics::LyricsLine {
        start_ms: start,
        end_ms: end,
        en: "x".into(),
        sk: None,
        words: None,
    }
}

#[test]
fn sanitize_clamps_start_below_floor_up_to_floor() {
    let out = sanitize_lines(vec![line(0, 500), line(200, 800)]);
    assert_eq!(out[0].start_ms, 0);
    assert_eq!(out[0].end_ms, 500);
    assert_eq!(out[1].start_ms, 500, "start below floor clamps up to floor");
    assert_eq!(out[1].end_ms, 800);
}

#[test]
fn sanitize_does_not_clamp_start_above_floor() {
    let out = sanitize_lines(vec![line(0, 500), line(600, 900)]);
    assert_eq!(out[1].start_ms, 600, "start above floor is left alone");
}

#[test]
fn sanitize_clamps_end_below_minimum_duration() {
    let out = sanitize_lines(vec![line(1000, 1050)]);
    assert_eq!(out[0].start_ms, 1000);
    assert_eq!(out[0].end_ms, 1200, "50ms line clamped up to start+200");
}

#[test]
fn sanitize_end_far_above_minimum_is_unchanged() {
    let out = sanitize_lines(vec![line(10, 500)]);
    assert_eq!(
        out[0].end_ms, 500,
        "end far above minimum must not be clamped"
    );
}

#[test]
fn sanitize_start_exactly_at_floor_not_clamped() {
    let out = sanitize_lines(vec![line(0, 500), line(500, 900)]);
    assert_eq!(out[1].start_ms, 500);
}

#[test]
fn sanitize_end_exactly_at_min_duration_not_clamped() {
    let out = sanitize_lines(vec![line(100, 300)]); // 300 == 100 + MIN(200)
    assert_eq!(out[0].end_ms, 300);
}

#[test]
fn sanitize_empty_input_empty_output() {
    assert!(sanitize_lines(vec![]).is_empty());
}

// --- build_track: the base-tier decision the worker routes on ---

#[test]
fn build_track_none_on_empty_words() {
    assert!(build_track(&[], 22).is_none());
}

#[test]
fn build_track_none_on_all_blank_words() {
    let words = vec![w("   ", 0, 100), w("", 200, 300)];
    assert!(build_track(&words, 22).is_none());
}

#[test]
fn build_track_some_on_real_words() {
    let words = vec![
        w("holy", 0, 500),
        w("holy", 600, 1100),
        w("lord", 1200, 1700),
    ];
    let t = build_track(&words, 22).expect("real words → Some track");
    assert_eq!(t.version, 22);
    assert_eq!(t.source, "gemini-3-5-transcribe");
    assert_eq!(t.language_source, "en");
    assert!(
        t.language_translation.is_empty(),
        "sk filled by shared tail, not here"
    );
    assert!(!t.lines.is_empty());
    assert!(t.lines.iter().all(|l| l.sk.is_none() && l.words.is_none()));
}
