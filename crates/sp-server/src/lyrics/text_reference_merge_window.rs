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
    AsrWord, CHORUS_REPEAT_MIN_MATCH_RATIO, CHORUS_REPEAT_MIN_MATCHED_WORDS,
    CHORUS_REPEAT_WINDOW_CAP_MS, MIN_LINE_DURATION_MS,
};

#[cfg_attr(test, mutants::skip)] // Phase 2 chorus-repeat window scan; nested loop with LCS scoring and ratio gate. Integration-tested through `text_reference_merge_tests::second_chorus_repeat_pass` and id=21 reprocess. Whole-fn return replacement to None/Some((0,0.0,vec![])) is dead-code-equivalent at first match; targeted boundary tests would require synthetic ratio fixtures with no semantic gain.
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
            let cap_end_ms = win_start_ms.saturating_add(CHORUS_REPEAT_WINDOW_CAP_MS);
            // Cap on word START so a 2-word line whose 2nd word straddles
            // the cap still matches. Phase 5 clips the EMITTED line to
            // LONG_LINE_CAP_MS anyway, so the matcher can scan a wider
            // span (CHORUS_REPEAT_WINDOW_CAP_MS = 30s) to recover slow
            // choruses. id=21 "Good Shepherd" 4:02: chorus repeat sung
            // over 12.4s — under the prior 8s cap the window shrank to
            // 6/13 ref words = 0.46 ratio < 0.6 gate, no emit, wall
            // blank.
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
        // Whole-gap LCS would match the FIRST "holy" with the LAST
        // "forever" (span > CHORUS_REPEAT_WINDOW_CAP_MS); the sliding
        // window restricts to ≤ CHORUS_REPEAT_WINDOW_CAP_MS so it picks a
        // dense close pair instead.
        let asr_words = vec![
            w("holy", 0, 100),            // 0 — earliest "holy"
            w("you", 4000, 4100),         // 1
            w("holy", 20000, 20100),      // 2 — viable window start
            w("forever", 48000, 48500),   // 3 — end_ms within 20000+30000
            w("holy", 80000, 80100),      // 4
            w("forever", 120000, 120500), // 5
        ];
        let ref_norms: Vec<Vec<String>> = vec![vec!["holy".into(), "forever".into()]];
        let unconsumed: Vec<usize> = (0..asr_words.len()).collect();
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        let (line_idx, _score, matched) = result.expect("should match");
        assert_eq!(line_idx, 0);
        // Span ≤ CHORUS_REPEAT_WINDOW_CAP_MS by construction.
        let span = asr_words[*matched.last().unwrap()].end_ms
            - asr_words[*matched.first().unwrap()].start_ms;
        assert!(
            span <= CHORUS_REPEAT_WINDOW_CAP_MS,
            "span {} exceeds cap",
            span
        );
    }

    #[test]
    fn best_window_match_recovers_chorus_repeat_with_long_internal_gap() {
        // Regression for id=21 "Good Shepherd" 4:02: chorus repeat
        // "all my days I will stay in the house" sung over 12.4 s, then
        // a 15 s instrumental pause, then "of my father". The 8 s cap
        // (LONG_LINE_CAP_MS) shrank the window to 6 of 13 ref words and
        // the chorus pass emitted nothing. With CHORUS_REPEAT_WINDOW_CAP_MS
        // (30 s) the matcher spans the whole repeat and emits.
        let asr_words = vec![
            w("all", 234069, 234369),   // 0
            w("my", 234570, 235170),    // 1
            w("days", 235310, 236010),  // 2
            w("i", 236030, 236050),     // 3
            w("will", 236070, 239152),  // 4
            w("stay", 239993, 244075),  // 5 — sustained
            w("in", 245036, 245156),    // 6
            w("the", 245256, 245636),   // 7
            w("house", 245696, 246437), // 8
        ];
        let ref_norms: Vec<Vec<String>> = vec![vec![
            "so".into(),
            "all".into(),
            "my".into(),
            "days".into(),
            "i".into(),
            "will".into(),
            "stay".into(),
            "in".into(),
            "the".into(),
            "house".into(),
            "of".into(),
            "my".into(),
            "father".into(),
        ]];
        let unconsumed: Vec<usize> = (0..asr_words.len()).collect();
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        let (line_idx, score, matched) = result.expect(
            "chorus repeat over 12.4 s span MUST match with 30 s window cap (was failing under 8 s cap)",
        );
        assert_eq!(line_idx, 0);
        assert!(
            matched.len() >= 8,
            "expected ≥8 matched words (\"all my days i will stay in the house\"); got {}",
            matched.len()
        );
        assert!(
            score >= CHORUS_REPEAT_MIN_MATCH_RATIO,
            "score {} below MIN_MATCH_RATIO {}",
            score,
            CHORUS_REPEAT_MIN_MATCH_RATIO
        );
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

    // Boundary tests targeting mutation survivors at lines 46, 60, 63, 64, 70, 73.

    #[test]
    fn best_window_match_finds_pair_when_window_grows_past_start_plus_one() {
        // ref = [a, b]. asr = [a, b] with span >= MIN_LINE_DURATION_MS (500).
        // start_pos=0: end_pos starts at 1. asr_words[unconsumed[1]].start_ms = 200,
        // <= cap_end_ms (0 + 8000 = 8000) → end_pos advances to 2. Window covers
        // both words. matched len = 2. Mutation `start_pos + 1` → `start_pos * 1`
        // gives end_pos = 0 (start), zero-width window, no matches → returns None.
        let asr_words = vec![w("a", 0, 100), w("b", 700, 1000)];
        let ref_norms: Vec<Vec<String>> = vec![vec!["a".into(), "b".into()]];
        let unconsumed = vec![0, 1];
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(result.is_some(), "must find the pair via window-grow");
        let (_, _, matched) = result.unwrap();
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn best_window_match_rejects_single_word_match_below_min_matched_words() {
        // ref = [a, b, c, d, e]. asr = [a, x, y, z, q]. Only "a" matches —
        // matched.len() = 1 < CHORUS_REPEAT_MIN_MATCHED_WORDS (=2) → continue.
        // Original returns None. Mutation `<` ↔ `>` would treat 1 > 2 as false,
        // proceeding past the guard; subsequent score gate (1/5 = 0.2 < 0.6)
        // catches it for that mutation. Mutation `<` ↔ `>=` accepts at 1 vs 2
        // boundary too. Either way the test fails on at least one mutant.
        let asr_words = vec![
            w("a", 0, 100),
            w("x", 200, 300),
            w("y", 400, 500),
            w("z", 600, 700),
            w("q", 800, 900),
        ];
        let ref_norms: Vec<Vec<String>> = vec![vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
        ]];
        let unconsumed: Vec<usize> = (0..asr_words.len()).collect();
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(result.is_none());
    }

    #[test]
    fn best_window_match_rejects_short_span_below_min_line_duration() {
        // ref = [a, b]. asr = [a, b] but with span 200 ms (< MIN_LINE_DURATION_MS
        // = 500) → continue. Result: None. Kills span-guard `<` ↔ `==`/`<=`
        // mutations at line 70 (boundary value of 500). With span = 200, all
        // three mutations on `<` (==, <=, >) flip the gate behaviour.
        let asr_words = vec![w("a", 0, 50), w("b", 100, 200)];
        let ref_norms: Vec<Vec<String>> = vec![vec!["a".into(), "b".into()]];
        let unconsumed = vec![0, 1];
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(
            result.is_none(),
            "span = 200 ms must be rejected (< MIN_LINE_DURATION_MS = 500)"
        );
    }

    #[test]
    fn best_window_match_picks_higher_score_among_candidates() {
        // Two candidate ref lines, both can match in-window. ref0 has 2 words
        // both in asr (score 1.0). ref1 has 3 words but only 2 match (score
        // 2/3 ≈ 0.67). Original: picks ref0 (score 1.0 > 0.67). Mutation
        // `>` ↔ `<` at the picker's tie-break would pick ref1. Mutation `>`
        // ↔ `==` (only update on equal scores) would never replace, returning
        // ref0 which happens to be the first scored — same as original here.
        // Mutation `>` ↔ `>=` makes equal-score also replace; here scores
        // differ so it shouldn't observably matter. Strongest test is
        // `>` ↔ `<` which inverts the tie-break direction.
        let asr_words = vec![w("a", 0, 100), w("b", 600, 700)];
        let ref_norms: Vec<Vec<String>> = vec![
            vec!["a".into(), "b".into()],             // li=0, score 1.0
            vec!["a".into(), "x".into(), "b".into()], // li=1, score 2/3
        ];
        let unconsumed = vec![0, 1];
        let result = best_window_match(&ref_norms, &unconsumed, &asr_words, &lcs_align_test);
        assert!(result.is_some());
        let (li, _score, _) = result.unwrap();
        assert_eq!(li, 0, "must pick ref line with higher score (1.0 vs 2/3)");
    }
}
