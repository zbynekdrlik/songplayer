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
//! It was never the gate's former line-anchor walk: measured on #144, a
//! one-word fallback anchor that jumped forward (the ASR heard "Where is our
//! angel" for "We raise our hands up") orphaned every line in between, which
//! reported a false 32 s uncovered stretch on a complete ★ text.
//!
//! The first-week review (#144) made it the gate's ONE alignment: `align`
//! computes it once and both halves read it — the sung coverage here, and
//! the reference → transcript line match in `reference_gate::match_lines`
//! (a line's share of words on the alignment and its first aligned word's
//! sung start).
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

/// One reference line on the alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineOnAlignment {
    /// The line's words (normalized, non-empty).
    pub words: usize,
    /// How many of them the alignment holds.
    pub aligned: usize,
    /// The sung start of the first of them; `None` when none is aligned.
    pub first_sung_start_ms: Option<u64>,
    /// Whether the line's OWN first word is on the alignment: only then is
    /// `first_sung_start_ms` where the line starts (a misheard first word
    /// leaves a later word's start).
    pub first_word_aligned: bool,
}

/// ONE order-preserving word alignment of a reference text against the sung
/// transcript, read by both halves of the gate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Alignment {
    /// Per reference line, in the lines' order.
    pub lines: Vec<LineOnAlignment>,
    /// What the text covers of the sung words.
    pub coverage: SungCoverage,
}

/// The pairs `(r, s)` — reference word `r` aligned to sung word `s` — of a
/// longest common subsequence of `reference` and `sung`, in order of both.
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
pub(crate) fn aligned_pairs(reference: &[String], sung: &[String]) -> Vec<(usize, usize)> {
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
    let mut pairs = Vec::new();
    let mut i = 0;
    for (j, sung_word) in sung.iter().enumerate() {
        i = (i..n)
            .find(|&r| reference[r] == *sung_word || suffix[at(r, j)] > suffix[at(r + 1, j)])
            .unwrap_or(n);
        if i < n && reference[i] == *sung_word {
            pairs.push((i, j));
            i += 1;
        }
    }
    pairs
}

/// `covered[j]` is true when sung word `j` is on the alignment
/// (`aligned_pairs`). Tests read the walk through it; production reads the
/// pairs in `align`.
#[cfg(test)]
pub(crate) fn covered_words(reference: &[String], sung: &[String]) -> Vec<bool> {
    let mut covered = vec![false; sung.len()];
    for (_, j) in aligned_pairs(reference, sung) {
        covered[j] = true;
    }
    covered
}

/// Align the text of `lines` with `words` (the sung transcript) once: per
/// line its words on the alignment, and what the text covers of the sung
/// words. Line timing plays no part: only the words and their order.
pub(crate) fn align(
    lines: &[crate::lyrics::reference_gate::AlignedLine],
    words: &[AsrWord],
) -> Alignment {
    let mut reference: Vec<String> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    // Each line's first word's index in `reference`.
    let mut first_word: Vec<usize> = Vec::with_capacity(lines.len());
    let mut per_line = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let line_words = normalized_words(&line.text);
        first_word.push(reference.len());
        per_line.push(LineOnAlignment {
            words: line_words.len(),
            aligned: 0,
            first_sung_start_ms: None,
            first_word_aligned: false,
        });
        owner.extend(std::iter::repeat_n(index, line_words.len()));
        reference.extend(line_words);
    }
    let sung: Vec<(&AsrWord, String)> = words
        .iter()
        .map(|w| (w, normalize_word(&w.text)))
        .filter(|(_, norm)| !norm.is_empty())
        .collect();
    let norms: Vec<String> = sung.iter().map(|(_, norm)| norm.clone()).collect();
    let mut covered = vec![false; sung.len()];
    for (r, s) in aligned_pairs(&reference, &norms) {
        covered[s] = true;
        let on = &mut per_line[owner[r]];
        on.aligned += 1;
        if on.first_sung_start_ms.is_none() {
            on.first_sung_start_ms = Some(sung[s].0.start_ms);
        }
        if r == first_word[owner[r]] {
            on.first_word_aligned = true;
        }
    }
    Alignment {
        lines: per_line,
        coverage: coverage_of(&sung, &covered),
    }
}

/// Measure how much of `words` (the sung transcript) the text of `lines`
/// covers (`align`'s coverage).
pub fn sung_coverage(
    lines: &[crate::lyrics::reference_gate::AlignedLine],
    words: &[AsrWord],
) -> SungCoverage {
    align(lines, words).coverage
}

/// The coverage numbers of the sung words `sung` whose alignment marks are
/// `covered`.
fn coverage_of(sung: &[(&AsrWord, String)], covered: &[bool]) -> SungCoverage {
    let covered_count = covered.iter().filter(|&&c| c).count();
    let mut max_uncovered_ms = 0;
    let mut run_start: Option<u64> = None;
    for ((word, _), &is_covered) in sung.iter().zip(covered) {
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
