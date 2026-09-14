//! Mutation-killing unit tests for `reference_gate.rs` pure helpers
//! (`normalize_word`, `find_ngram`, `match_lines`, `median_i64`, `evaluate`).
//!
//! Wired into `reference_gate.rs` as a sibling `#[path]` test module. Each
//! test asserts an EXACT value at the precise input that flips under one
//! mutation, so it passes on the unmutated code and fails under that
//! mutant. Helpers mirror the style of the existing `mod tests` block.

use super::*;

fn line(text: &str, start_ms: u64) -> AlignedLine {
    AlignedLine {
        text: text.to_string(),
        start_ms,
    }
}

fn word(text: &str, start_ms: u64, end_ms: u64) -> AsrWord {
    AsrWord {
        text: text.to_string(),
        start_ms,
        end_ms,
    }
}

/// Push one phrase's words into `words`, one word per 300ms starting at
/// `start` — identical to the existing `mod tests` helper.
fn push_phrase(words: &mut Vec<AsrWord>, phrase: &str, start: u64) {
    let mut t = start;
    for tok in phrase.split_whitespace() {
        words.push(word(tok, t, t + 300));
        t += 300;
    }
}

fn norms(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// -------------------------------------------------------------------------
// normalize_word — line 68: `*c == '\''`
// -------------------------------------------------------------------------

/// Kills `68:47 == -> !=`. Under `!=` the filter keeps every non-apostrophe
/// (punctuation survives) and DROPS the apostrophe, so "don't" would
/// normalize to "dont". The unmutated code keeps the apostrophe.
#[test]
fn normalize_word_keeps_apostrophe_exact() {
    assert_eq!(normalize_word("don't"), "don't");
}

// -------------------------------------------------------------------------
// find_ngram — line 87: `if n == 0 || cursor + n > word_norms.len()`
// -------------------------------------------------------------------------

/// Kills `87:15 || -> &&`. With an empty ngram (n == 0) the unmutated guard
/// short-circuits to `None`. Under `&&` the guard is `0 == 0 && 0 > 5` =
/// false, so the search runs and an empty ngram matches at position 0 →
/// `Some(0)`.
#[test]
fn find_ngram_empty_ngram_is_none_not_some() {
    let w = norms(&["a", "b", "c", "d", "e"]);
    let empty: Vec<String> = Vec::new();
    assert_eq!(find_ngram(&w, 0, &empty), None);
}

/// Kills three co-located mutants on line 87 with one exact-fit boundary
/// case: cursor = 3, n = 2, len = 5, ngram = ["d","e"] at position 3.
///   - `87:25 + -> *`: `3 * 2 = 6 > 5` → early `None`.
///   - `87:29 > -> ==`: `3 + 2 == 5` → early `None`.
///   - `87:29 > -> >=`: `3 + 2 >= 5` → early `None`.
///     The unmutated guard is `5 > 5` = false, so it finds the match → `Some(3)`.
#[test]
fn find_ngram_exact_fit_at_end_matches_some() {
    let w = norms(&["a", "b", "c", "d", "e"]);
    let ngram = norms(&["d", "e"]);
    assert_eq!(find_ngram(&w, 3, &ngram), Some(3));
}

// -------------------------------------------------------------------------
// match_lines — line 125: `cursor = pos + 1;`
// -------------------------------------------------------------------------

/// Kills `125:30 + -> *` (`pos * 1` == `pos`, cursor never advances past the
/// matched anchor). Two identical lines; the phrase appears twice in the
/// ASR stream. The unmutated cursor advances past the first occurrence so
/// the SECOND line binds to the later ASR start (5000). Under `pos * 1` the
/// cursor stays at 0 and the second line re-binds to the earlier 1000.
#[test]
fn match_lines_repeated_line_binds_second_occurrence_forward() {
    let lines = vec![line("we lift you", 1000), line("we lift you", 5000)];
    let mut words = Vec::new();
    push_phrase(&mut words, "we lift you", 1000);
    push_phrase(&mut words, "we lift you", 5000);

    let matches = match_lines(&lines, &words);
    assert_eq!(matches, vec![Some(1000), Some(5000)]);
}

// -------------------------------------------------------------------------
// median_i64 — lines 144/145/147
// -------------------------------------------------------------------------

/// Odd-length (n = 5) median. Unmutated: `5 % 2 == 1` → `sorted[2]` == 20.
/// One test kills FOUR mutants, each of which diverts to the even branch or
/// a wrong index:
///   - `144:10 % -> +`: `5 + 2 == 1` false → even branch → 15.
///   - `144:10 % -> /`: `5 / 2 == 1` false → even branch → 15.
///   - `144:14 == -> !=`: `5 % 2 != 1` false → even branch → 15.
///   - `145:18 / -> %` (odd branch index): `sorted[5 % 2]` == `sorted[1]` == 10.
#[test]
fn median_i64_odd_len_five_is_middle_element() {
    assert_eq!(median_i64(&[0, 10, 20, 30, 40]), 20);
}

/// Even-length (n = 4) median. Unmutated: avg of `sorted[1]`=10 and
/// `sorted[2]`=20 → round(15.0) = 15. Kills `147:30 - -> /`: `sorted[n/2/1]`
/// == `sorted[2]` == 20, giving avg(20, 20) = 20 instead of 15.
#[test]
fn median_i64_even_len_four_averages_two_middles() {
    assert_eq!(median_i64(&[0, 10, 20, 30]), 15);
}

// -------------------------------------------------------------------------
// evaluate — boundary mutants on lines 180/186/200/206/212
// -------------------------------------------------------------------------

/// Kills `180:39 > -> >=`. With zero lines, `lines_total >= 0` is always
/// true, so the mutant computes `0.0 / 0.0` = NaN for matched_frac; the
/// unmutated `lines_total > 0` guard yields exactly 0.0. Verdict is
/// Coverage either way (short-circuit on `lines_total == 0`), so we pin the
/// stat.
#[test]
fn evaluate_zero_lines_matched_frac_is_exactly_zero() {
    let lines: Vec<AlignedLine> = vec![];
    let words: Vec<AsrWord> = vec![];
    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            stats,
        } => {
            assert_eq!(stats.matched_frac, 0.0);
        }
        other => panic!("expected Fail(Coverage), got {other:?}"),
    }
}

/// Kills `186:44 > -> >=`. Two lines present, none match (empty ASR) →
/// lines_matched == 0. The mutant's `lines_matched >= 0` computes
/// `0.0 / 0.0` = NaN for within_400_frac; the unmutated guard yields exactly
/// 0.0. Verdict is Coverage either way, so we pin the stat.
#[test]
fn evaluate_no_matches_within_frac_is_exactly_zero() {
    let lines = vec![
        line("alpha bravo charlie", 1000),
        line("delta echo foxtrot", 2000),
    ];
    let words: Vec<AsrWord> = vec![];
    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            stats,
        } => {
            assert_eq!(stats.lines_total, 2);
            assert_eq!(stats.lines_matched, 0);
            assert_eq!(stats.within_400_frac, 0.0);
        }
        other => panic!("expected Fail(Coverage), got {other:?}"),
    }
}

/// Kills `200:41 < -> <=`. matched_frac lands EXACTLY on the 0.60 floor
/// (3 of 5 lines match, deltas 0 → median 0, within 1.0). Unmutated
/// `0.60 < 0.60` is false → the gate passes Coverage and reaches Pass. Under
/// `<=`, `0.60 <= 0.60` is true → Fail(Coverage).
#[test]
fn evaluate_matched_frac_exactly_060_passes() {
    let lines = vec![
        line("alpha bravo charlie", 1000),
        line("delta echo foxtrot", 2000),
        line("golf hotel india", 3000),
        line("missing line one here", 4000),
        line("missing line two here", 5000),
    ];
    let mut words = Vec::new();
    push_phrase(&mut words, "alpha bravo charlie", 1000);
    push_phrase(&mut words, "delta echo foxtrot", 2000);
    push_phrase(&mut words, "golf hotel india", 3000);

    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.lines_total, 5);
            assert_eq!(stats.lines_matched, 3);
            assert!((stats.matched_frac - 0.60).abs() < 1e-12);
            assert_eq!(stats.median_signed_ms, 0);
        }
        other => panic!("expected Pass exactly at the 0.60 coverage floor, got {other:?}"),
    }
}

/// Kills `206:31 > -> >=`. |median| lands EXACTLY on 400 (every matched line
/// is +400ms, still within the 400ms agreement window). Unmutated
/// `400 > 400` is false → Offset passes and the gate reaches Pass. Under
/// `>=`, `400 >= 400` is true → Fail(Offset).
#[test]
fn evaluate_median_offset_exactly_400_passes() {
    let lines = vec![
        line("alpha bravo charlie", 1000),
        line("delta echo foxtrot", 2000),
        line("golf hotel india", 3000),
    ];
    let mut words = Vec::new();
    push_phrase(&mut words, "alpha bravo charlie", 1400);
    push_phrase(&mut words, "delta echo foxtrot", 2400);
    push_phrase(&mut words, "golf hotel india", 3400);

    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.lines_matched, 3);
            assert_eq!(stats.median_signed_ms, 400);
            assert_eq!(stats.within_400_frac, 1.0);
        }
        other => panic!("expected Pass exactly at the 400ms offset boundary, got {other:?}"),
    }
}

/// Kills `212:24 < -> <=`. within_400_frac lands EXACTLY on 0.70 (10 matched
/// lines, 7 within 400ms at delta 0, 3 beyond at delta 900; median 0 so
/// Offset passes). Unmutated `0.70 < 0.70` is false → Agreement passes and
/// the gate reaches Pass. Under `<=`, `0.70 <= 0.70` is true →
/// Fail(Agreement).
#[test]
fn evaluate_within_frac_exactly_070_passes() {
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for i in 0..10u64 {
        let base = 1000 + i * 10_000;
        let text = format!("word{i}a word{i}b word{i}c");
        lines.push(line(&text, base));
        // First 7 lines match at delta 0 (within 400ms); last 3 at +900ms.
        let delta = if i < 7 { 0 } else { 900 };
        push_phrase(&mut words, &text, base + delta);
    }

    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.lines_matched, 10);
            assert_eq!(stats.median_signed_ms, 0);
            assert!((stats.within_400_frac - 0.70).abs() < 1e-12);
        }
        other => panic!("expected Pass exactly at the 0.70 agreement boundary, got {other:?}"),
    }
}
