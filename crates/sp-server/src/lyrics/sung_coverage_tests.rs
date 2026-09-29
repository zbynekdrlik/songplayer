//! Unit tests for `sung_coverage.rs` (#144). The exact masks were derived
//! with a scratch model of the walk (and a mutation harness over it): each
//! case pins a decision some mutation of the fill or the walk gets wrong.

use super::*;
use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::reference_gate::AlignedLine;

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn mask(reference: &str, sung: &str) -> Vec<bool> {
    covered_words(&words(reference), &words(sung))
}

fn line(text: &str) -> AlignedLine {
    AlignedLine {
        text: text.to_string(),
        start_ms: 0,
    }
}

fn word(text: &str, start_ms: u64, end_ms: u64) -> AsrWord {
    AsrWord {
        text: text.to_string(),
        start_ms,
        end_ms,
    }
}

#[test]
fn identical_text_covers_every_sung_word() {
    assert_eq!(mask("a b c", "a b c"), vec![true, true, true]);
}

#[test]
fn a_sung_word_the_text_lacks_stays_uncovered() {
    assert_eq!(mask("a b c", "a x b c"), vec![true, false, true, true]);
}

#[test]
fn a_text_word_nobody_sings_is_skipped() {
    assert_eq!(mask("a b x c", "a b c"), vec![true, true, true]);
}

#[test]
fn an_empty_text_covers_nothing() {
    assert_eq!(mask("", "a b"), vec![false, false]);
}

#[test]
fn nothing_sung_gives_an_empty_mask() {
    assert_eq!(mask("a b", ""), Vec::<bool>::new());
}

/// A chorus the text holds once covers ONE sung repetition, never two: the
/// alignment is order-preserving, each text word binds once.
#[test]
fn a_chorus_written_once_covers_one_sung_repetition() {
    assert_eq!(
        mask("we lift you up", "we lift you up we lift you up"),
        vec![true, true, true, true, false, false, false, false]
    );
}

/// Greedy "earliest match" would bind "a" to the first sung word and lose
/// "b"; the longest common subsequence keeps both.
#[test]
fn the_alignment_is_the_longest_not_the_earliest() {
    assert_eq!(mask("b a", "a b a"), vec![false, true, true]);
}

#[test]
fn a_leading_extra_sung_word_is_dropped_before_the_text() {
    assert_eq!(mask("a b", "b a b"), vec![false, true, true]);
}

#[test]
fn a_partial_text_leaves_the_rest_of_the_song_uncovered() {
    assert_eq!(
        mask("a b", "a b c d e"),
        vec![true, true, false, false, false]
    );
}

/// The only match available is the text's LAST word with the first sung
/// word; the walk must skip both leading "a"s to reach it.
#[test]
fn the_walk_skips_text_words_to_reach_a_later_match() {
    assert_eq!(mask("a a b", "b a"), vec![true, false]);
}

#[test]
fn the_walk_skips_two_text_words_for_the_one_match() {
    assert_eq!(mask("a b c", "c a"), vec![true, false]);
}

/// 4 sung words, the text covers 2 (0.5): "sweet", and the one transcript
/// token "the sound", stay uncovered and run together 1 000 → 2 500 ms.
/// Case and trailing punctuation do not matter ("Amazing", "grace,").
#[test]
fn sung_coverage_counts_the_covered_share_and_the_uncovered_run() {
    let lines = vec![line("amazing grace"), line("the sound")];
    let sung = vec![
        word("Amazing", 0, 500),
        word("grace,", 500, 1_000),
        word("sweet", 1_000, 2_000),
        word("the sound", 2_000, 2_500),
    ];
    // "the sound" is ONE transcript token that normalizes to "thesound",
    // which neither of the text's two words equals.
    let got = sung_coverage(&lines, &sung);
    assert_eq!(got.sung_words, 4);
    assert_eq!(got.covered_frac, 0.5);
    assert_eq!(got.max_uncovered_ms, 1_500);
}

/// A covered word between two uncovered stretches ends the first run: the
/// longest run is ONE of them (1 000 ms), never their joined span (11 000).
#[test]
fn a_covered_word_ends_an_uncovered_run() {
    let lines = vec![line("amen")];
    let sung = vec![
        word("oh", 0, 1_000),
        word("amen", 1_000, 2_000),
        word("yeah", 10_000, 11_000),
    ];
    let got = sung_coverage(&lines, &sung);
    assert_eq!(got.sung_words, 3);
    assert_eq!(got.max_uncovered_ms, 1_000);
}

/// A run's span is its first word start → its last word end, over every
/// uncovered word in between.
#[test]
fn an_uncovered_run_spans_first_start_to_last_end() {
    let lines = vec![line("amen")];
    let sung = vec![
        word("amen", 0, 500),
        word("oh", 4_000, 4_500),
        word("my", 9_000, 9_400),
        word("soul", 30_000, 30_700),
    ];
    let got = sung_coverage(&lines, &sung);
    assert_eq!(got.max_uncovered_ms, 26_700);
    assert_eq!(got.covered_frac, 0.25);
}

/// A pure punctuation token is not singing: it is left out of the count
/// and never breaks or extends a run.
#[test]
fn punctuation_tokens_are_not_sung_words() {
    let lines = vec![line("a b")];
    let sung = vec![word("a", 0, 100), word("—", 100, 200), word("b", 200, 300)];
    let got = sung_coverage(&lines, &sung);
    assert_eq!(got.sung_words, 2);
    assert_eq!(got.covered_frac, 1.0);
    assert_eq!(got.max_uncovered_ms, 0);
}

#[test]
fn no_sung_word_covers_nothing_and_runs_nothing() {
    let got = sung_coverage(&[line("a b")], &[]);
    assert_eq!(
        got,
        SungCoverage {
            sung_words: 0,
            covered_frac: 0.0,
            max_uncovered_ms: 0,
        }
    );
}

/// 3 of 4 sung words covered: 0.75 exactly (a `*` or `%` in place of the
/// division gives 12.0 / 3.0).
#[test]
fn the_covered_share_is_covered_over_sung() {
    let lines = vec![line("a b c")];
    let sung = vec![
        word("a", 0, 100),
        word("b", 100, 200),
        word("x", 200, 300),
        word("c", 300, 400),
    ];
    let got = sung_coverage(&lines, &sung);
    assert_eq!(got.covered_frac, 0.75);
    assert_eq!(got.max_uncovered_ms, 100);
}
