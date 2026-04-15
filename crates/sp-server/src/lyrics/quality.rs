//! Pure functions that compute quality metrics on aligned lyric lines.
//!
//! Used by `worker.rs` for `warn!` logs when a line comes back degenerate
//! (e.g. 100% duplicate word starts) and by the E2E post-deploy test to
//! hard-assert #148 alignment quality.

use sp_core::lyrics::LyricsLine;

/// Percentage of words whose `start_ms` equals their in-line predecessor's
/// `start_ms`. Range: 0.0–100.0. 0.0 = every word has a unique start.
/// Returns 0.0 for lines with < 2 words.
pub fn duplicate_start_pct(line: &LyricsLine) -> f64 {
    let Some(words) = line.words.as_ref() else {
        return 0.0;
    };
    if words.len() < 2 {
        return 0.0;
    }
    let mut duplicates = 0usize;
    for pair in words.windows(2) {
        if pair[1].start_ms == pair[0].start_ms {
            duplicates += 1;
        }
    }
    let denom = (words.len() - 1) as f64;
    100.0 * (duplicates as f64) / denom
}

/// Sample standard deviation of inter-word gap durations (ms).
///
/// A line whose aligner produced perfectly even spacing (band-aid /
/// synthesized timings) collapses to stddev ≈ 0. Real singing produces
/// irregular phonetic gaps with stddev ≥ 50 ms on typical worship vocals.
/// Returns 0.0 for lines with < 3 words (need at least 2 gaps).
pub fn gap_stddev_ms(line: &LyricsLine) -> f64 {
    let Some(words) = line.words.as_ref() else {
        return 0.0;
    };
    if words.len() < 3 {
        return 0.0;
    }
    let mut gaps: Vec<f64> = Vec::with_capacity(words.len() - 1);
    for pair in words.windows(2) {
        gaps.push((pair[1].start_ms as f64) - (pair[0].start_ms as f64));
    }
    let mean = gaps.iter().sum::<f64>() / (gaps.len() as f64);
    let variance: f64 = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / (gaps.len() as f64);
    variance.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsWord};

    fn line_with_words(words: &[(u64, u64, &str)]) -> LyricsLine {
        LyricsLine {
            start_ms: words.first().map(|w| w.0).unwrap_or(0),
            end_ms: words.last().map(|w| w.1).unwrap_or(0),
            en: String::new(),
            sk: None,
            words: Some(
                words
                    .iter()
                    .map(|(s, e, t)| LyricsWord {
                        start_ms: *s,
                        end_ms: *e,
                        text: (*t).into(),
                    })
                    .collect(),
            ),
        }
    }

    fn line_no_words() -> LyricsLine {
        LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "nope".into(),
            sk: None,
            words: None,
        }
    }

    // ---------- duplicate_start_pct ----------

    #[test]
    fn duplicate_start_pct_zero_for_progressive_words() {
        let l = line_with_words(&[(0, 100, "a"), (200, 300, "b"), (400, 500, "c")]);
        assert!((duplicate_start_pct(&l) - 0.0).abs() < 0.001);
    }

    #[test]
    fn duplicate_start_pct_fully_collapsed_is_100_pct() {
        let l = line_with_words(&[(100, 200, "a"), (100, 200, "b"), (100, 200, "c")]);
        assert!((duplicate_start_pct(&l) - 100.0).abs() < 0.001);
    }

    #[test]
    fn duplicate_start_pct_half_collapsed_is_50_pct() {
        // 4 words, 3 pairs. pair (1,2) shares start_ms; (0,1) and (2,3) do not.
        // => 1/3 ≈ 33.33 %, not 50 %.
        let l = line_with_words(&[
            (0, 100, "a"),
            (200, 300, "b"),
            (200, 300, "c"),
            (500, 600, "d"),
        ]);
        let pct = duplicate_start_pct(&l);
        assert!(
            (pct - (100.0 / 3.0)).abs() < 0.01,
            "expected ~33.33 %, got {pct}"
        );
    }

    #[test]
    fn duplicate_start_pct_no_words_returns_zero() {
        assert_eq!(duplicate_start_pct(&line_no_words()), 0.0);
    }

    #[test]
    fn duplicate_start_pct_single_word_returns_zero() {
        let l = line_with_words(&[(0, 100, "one")]);
        assert_eq!(duplicate_start_pct(&l), 0.0);
    }

    /// Boundary: exactly 2 words must produce a real percentage, not 0.
    /// Catches a `< 2` → `<= 2` mutation that would wrongly zero this case.
    #[test]
    fn duplicate_start_pct_two_words_progressive_returns_zero() {
        let l = line_with_words(&[(0, 100, "a"), (200, 300, "b")]);
        assert_eq!(
            duplicate_start_pct(&l),
            0.0,
            "two non-duplicate words must not be short-circuited as 'too few'"
        );
    }

    /// Boundary: exactly 2 words sharing start_ms must be 100% duplicate,
    /// not 0%. Catches the same `< 2` → `<= 2` mutation more aggressively.
    #[test]
    fn duplicate_start_pct_two_words_collapsed_returns_100_pct() {
        let l = line_with_words(&[(50, 100, "a"), (50, 100, "b")]);
        assert!(
            (duplicate_start_pct(&l) - 100.0).abs() < 0.001,
            "two duplicate-start words must be 100% — would silently report 0% under <= 2 mutation"
        );
    }

    // ---------- gap_stddev_ms ----------

    #[test]
    fn gap_stddev_ms_zero_for_perfectly_even_gaps() {
        let l = line_with_words(&[
            (0, 50, "a"),
            (100, 150, "b"),
            (200, 250, "c"),
            (300, 350, "d"),
        ]);
        // All gaps == 100 ms → stddev 0.
        assert!(gap_stddev_ms(&l).abs() < 0.001);
    }

    #[test]
    fn gap_stddev_ms_positive_for_irregular_gaps() {
        // gaps: 100, 300, 200. mean 200. variance = (10000 + 10000 + 0) / 3.
        // stddev ~= 81.65 ms
        let l = line_with_words(&[
            (0, 50, "a"),
            (100, 150, "b"),
            (400, 450, "c"),
            (600, 650, "d"),
        ]);
        let s = gap_stddev_ms(&l);
        assert!((s - 81.65).abs() < 1.0, "expected ~81.65 ms, got {s}");
    }

    #[test]
    fn gap_stddev_ms_fewer_than_three_words_returns_zero() {
        let l = line_with_words(&[(0, 100, "a"), (200, 300, "b")]);
        assert_eq!(gap_stddev_ms(&l), 0.0);
    }

    /// Boundary: exactly 3 words must produce a real stddev, not 0.
    /// Catches `< 3` → `<= 3` and `< 3` → `== 3` mutations that would
    /// silently zero this case (3 words = 2 gaps, the minimum needed
    /// for a meaningful sample stddev).
    #[test]
    fn gap_stddev_ms_three_words_with_unequal_gaps_is_positive() {
        // gaps: 100, 300. mean 200. variance = (10000 + 10000) / 2 = 10000.
        // stddev = 100.
        let l = line_with_words(&[(0, 50, "a"), (100, 150, "b"), (400, 450, "c")]);
        let s = gap_stddev_ms(&l);
        assert!(
            (s - 100.0).abs() < 0.001,
            "expected stddev 100 ms for 3-word line with gaps (100, 300); got {s} — \
             a `<= 3` or `== 3` mutation in the guard would short-circuit to 0.0"
        );
    }

    #[test]
    fn gap_stddev_ms_no_words_returns_zero() {
        assert_eq!(gap_stddev_ms(&line_no_words()), 0.0);
    }
}
