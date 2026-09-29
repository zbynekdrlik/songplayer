//! The ASR → reference half of the reference gate (#144): how much of what
//! is SUNG — the independent Gemini 3.5 Transcribe word stream — a reference
//! text covers.
//!
//! `reference_gate::match_lines` asks the opposite question: is each
//! reference line found in the transcript, on time? A partial lyric (a
//! YouTube description holding a third of the song) answers yes for every
//! line it has, passes as ★, and mtl then stretches its lines over the
//! singing it lacks — song 286 held "I believe in the Gospel" on the wall
//! for 48 s while the singers sang other lines.
//!
//! Alignment: an order-preserving word alignment (the longest common
//! subsequence) of ALL the reference words against ALL the transcript words,
//! with the gate's own `normalize_word`. It stays monotonic, so a repeated
//! chorus in the text binds to one sung repetition each and never twice.
//! It is deliberately not the gate's line-anchor walk: measured on #144, a
//! one-word fallback anchor that jumps forward (the ASR heard "Where is our
//! angel" for "We raise our hands up") orphans every line in between, which
//! reported a false 32 s uncovered stretch on a complete ★ text.
//!
//! Pure; the thresholds that turn these numbers into a verdict live next to
//! the other gate constants in `reference_gate.rs`.

use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::reference_gate::{normalize_word, normalized_words};

/// What a reference text covers of the sung word stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SungCoverage {
    /// Transcript words that carry a word (normalize to non-empty); pure
    /// punctuation tokens are not singing and are left out.
    pub sung_words: usize,
    /// Share of `sung_words` the alignment covers (`0.0` for no sung word).
    pub covered_frac: f64,
    /// The longest run of consecutive uncovered sung words, from the run's
    /// first word start to its last word end (`0` when every word is covered).
    pub max_uncovered_ms: u64,
}

/// `covered[j]` is true when sung word `j` is aligned to a reference word by
/// a longest common subsequence of `reference` and `sung`.
///
/// Fill: `suffix[at(i, j)]` = LCS length of `reference[i..]` and `sung[j..]`.
/// Walk: for each sung word, skip the reference words an optimal alignment
/// drops before it; then either the next reference word IS this sung word
/// (matched — matching equal words is always optimal) or the optimum drops
/// this sung word. For an unequal pair the optimum must drop the sung word
/// exactly when dropping the reference word would lose a match
/// (`suffix[at(r, j)] > suffix[at(r + 1, j)]`); where both drops keep the
/// optimum, the walk drops the reference word and keeps the sung word for
/// the next reference word.
pub(crate) fn covered_words(reference: &[String], sung: &[String]) -> Vec<bool> {
    let n = reference.len();
    let width = sung.len() + 1;
    let at = |i: usize, j: usize| i * width + j;
    let mut suffix = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..sung.len()).rev() {
            suffix[at(i, j)] = if reference[i] == sung[j] {
                suffix[at(i + 1, j + 1)] + 1
            } else {
                suffix[at(i + 1, j)].max(suffix[at(i, j + 1)])
            };
        }
    }
    let mut covered = vec![false; sung.len()];
    let mut i = 0;
    for (j, sung_word) in sung.iter().enumerate() {
        i = (i..n)
            .find(|&r| reference[r] == *sung_word || suffix[at(r, j)] > suffix[at(r + 1, j)])
            .unwrap_or(n);
        if i < n && reference[i] == *sung_word {
            covered[j] = true;
            i += 1;
        }
    }
    covered
}

/// Measure how much of `words` (the sung transcript) the text of `lines`
/// covers. Line timing plays no part: only the words and their order.
pub fn sung_coverage(
    lines: &[crate::lyrics::reference_gate::AlignedLine],
    words: &[AsrWord],
) -> SungCoverage {
    let reference: Vec<String> = lines
        .iter()
        .flat_map(|l| normalized_words(&l.text))
        .collect();
    let sung: Vec<(&AsrWord, String)> = words
        .iter()
        .map(|w| (w, normalize_word(&w.text)))
        .filter(|(_, norm)| !norm.is_empty())
        .collect();
    let norms: Vec<String> = sung.iter().map(|(_, norm)| norm.clone()).collect();
    let covered = covered_words(&reference, &norms);

    let covered_count = covered.iter().filter(|&&c| c).count();
    let mut max_uncovered_ms = 0;
    let mut run_start: Option<u64> = None;
    for ((word, _), &is_covered) in sung.iter().zip(&covered) {
        if is_covered {
            run_start = None;
            continue;
        }
        let start = *run_start.get_or_insert(word.start_ms);
        max_uncovered_ms = max_uncovered_ms.max(word.end_ms.saturating_sub(start));
    }
    SungCoverage {
        sung_words: sung.len(),
        covered_frac: covered_count as f64 / sung.len().max(1) as f64,
        max_uncovered_ms,
    }
}

#[cfg(test)]
#[path = "sung_coverage_tests.rs"]
mod tests;
