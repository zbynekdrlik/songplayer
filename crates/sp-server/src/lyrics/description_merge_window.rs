//! Sliding-window matcher for Phase 2 chorus-repeat detection.
//!
//! Old whole-gap LCS picked any subsequence of words that matched the
//! ref-line pattern, regardless of audio span. On repetitive worship songs
//! ("your name stands above them all" sung 4× in a bridge) it would match
//! 5 contiguous words from one chorus + a 6th from a different chorus 7 s
//! later — emit's pre-Phase-5 span exceeded `LONG_LINE_CAP_MS`, the cap
//! truncated, the next emit's start_ms got floor-clamped past its real
//! word time, the wall displayed lines late.
//!
//! New algorithm: for every (ref_line × candidate_start) pair, build a
//! window of unconsumed indices whose end_ms fits
//! `start_ms + LONG_LINE_CAP_MS`, LCS-align ref words to window words,
//! score `matched.len() / ref_words.len()`. Return the highest-scoring
//! tuple meeting `CHORUS_REPEAT_MIN_*` thresholds. Match `Vec` is sorted
//! ascending and span-bounded by construction, so derived
//! `(min.start_ms, max.end_ms)` is guaranteed ≤ cap.

use super::{
    AsrWord, CHORUS_REPEAT_MIN_MATCH_RATIO, CHORUS_REPEAT_MIN_MATCHED_WORDS, LONG_LINE_CAP_MS,
    MIN_LINE_DURATION_MS,
};

pub(super) fn best_window_match(
    ref_norms_per_line: &[Vec<String>],
    unconsumed: &[usize],
    asr_words: &[AsrWord],
    lcs_align: &impl Fn(&[&str], &[&str]) -> Vec<Option<usize>>,
) -> Option<(usize, f32, Vec<usize>)> {
    let mut best: Option<(usize, f32, Vec<usize>)> = None;
    for (li, ref_norms) in ref_norms_per_line.iter().enumerate() {
        if ref_norms.is_empty() {
            continue;
        }
        let ref_strs: Vec<&str> = ref_norms.iter().map(|s| s.as_str()).collect();
        for start_pos in 0..unconsumed.len() {
            let win_start_ms = asr_words[unconsumed[start_pos]].start_ms;
            let cap_end_ms = win_start_ms.saturating_add(LONG_LINE_CAP_MS);
            // Cap on word START so a 2-word line whose 2nd word straddles
            // the cap still matches. The last word's end_ms may extend a
            // few hundred ms past the cap; Phase 5 will clip the line's
            // display duration to LONG_LINE_CAP_MS anyway. Without this,
            // id=132 4:20 "Holy forever": forever.end (262108) was 582ms
            // past holy(229).start + 8000 — window excluded forever, only
            // holy matched, < CHORUS_REPEAT_MIN_MATCHED_WORDS, no emit.
            let mut end_pos = start_pos + 1;
            while end_pos < unconsumed.len()
                && asr_words[unconsumed[end_pos]].start_ms <= cap_end_ms
            {
                end_pos += 1;
            }
            let window = &unconsumed[start_pos..end_pos];
            let window_norms: Vec<&str> =
                window.iter().map(|&i| asr_words[i].norm.as_str()).collect();
            let alignment = lcs_align(&ref_strs, &window_norms);
            let matched: Vec<usize> = alignment
                .iter()
                .filter_map(|a| a.map(|j| window[j]))
                .collect();
            if matched.len() < CHORUS_REPEAT_MIN_MATCHED_WORDS {
                continue;
            }
            let score = matched.len() as f32 / ref_norms.len() as f32;
            if score < CHORUS_REPEAT_MIN_MATCH_RATIO {
                continue;
            }
            let span_ms = asr_words[*matched.last().expect("non-empty")]
                .end_ms
                .saturating_sub(asr_words[*matched.first().expect("non-empty")].start_ms);
            if span_ms < MIN_LINE_DURATION_MS {
                continue;
            }
            if best.as_ref().is_none_or(|(_, s, _)| score > *s) {
                best = Some((li, score, matched));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcs_align_test(ref_w: &[&str], asr_w: &[&str]) -> Vec<Option<usize>> {
        let n = ref_w.len();
        let m = asr_w.len();
        if n == 0 || m == 0 {
            return vec![None; n];
        }
        let mut dp = vec![vec![0u32; m + 1]; n + 1];
        for i in 0..n {
            for j in 0..m {
                dp[i + 1][j + 1] = if ref_w[i] == asr_w[j] {
                    dp[i][j] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let mut alignment = vec![None; n];
        let mut i = n;
        let mut j = m;
        while i > 0 && j > 0 {
            if ref_w[i - 1] == asr_w[j - 1] {
                alignment[i - 1] = Some(j - 1);
                i -= 1;
                j -= 1;
            } else if dp[i - 1][j] >= dp[i][j - 1] {
                i -= 1;
            } else {
                j -= 1;
            }
        }
        alignment
    }

    fn w(text: &str, start: u32, end: u32) -> AsrWord {
        AsrWord {
            norm: text.into(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn best_window_match_picks_dense_close_window() {
        // Audio sequence with two viable "holy" + "forever" pairs and an
        // unrelated "holy" + "holy" stretch in between. Whole-gap LCS would
        // have matched the FIRST "holy" with the LAST "forever" (span > 8s);
        // sliding window restricts to windows ≤ LONG_LINE_CAP_MS so it
        // matches a dense close pair only.
        let asr_words = vec![
            w("holy", 0, 100),          // 0 — earliest "holy"
            w("you", 1000, 1100),       // 1
            w("holy", 5000, 5100),      // 2 — viable window start
            w("forever", 12000, 12500), // 3 — end_ms within 5000+8000
            w("holy", 20000, 20100),    // 4
            w("forever", 30000, 30500), // 5
        ];
        let ref_norms: Vec<Vec<String>> = vec![vec!["holy".into(), "forever".into()]];
        let unconsumed: Vec<usize> = (0..asr_words.len()).collect();
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        let (line_idx, _score, matched) = result.expect("should match");
        assert_eq!(line_idx, 0);
        // Span ≤ LONG_LINE_CAP_MS by construction.
        let span = asr_words[*matched.last().unwrap()].end_ms
            - asr_words[*matched.first().unwrap()].start_ms;
        assert!(span <= LONG_LINE_CAP_MS, "span {} exceeds cap", span);
    }

    #[test]
    fn best_window_match_rejects_below_min_match_ratio() {
        // Ref needs 3 words; window has only 1 match → ratio 0.33 < 0.6.
        let asr_words = vec![
            w("alpha", 0, 100),
            w("beta", 200, 300),
            w("gamma", 400, 500),
        ];
        let ref_norms: Vec<Vec<String>> = vec![vec!["alpha".into(), "x".into(), "y".into()]];
        let unconsumed: Vec<usize> = (0..asr_words.len()).collect();
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(result.is_none());
    }

    #[test]
    fn best_window_match_returns_none_when_empty_ref() {
        let asr_words = vec![w("a", 0, 100)];
        let ref_norms: Vec<Vec<String>> = vec![vec![]];
        let unconsumed = vec![0];
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(result.is_none());
    }
}
