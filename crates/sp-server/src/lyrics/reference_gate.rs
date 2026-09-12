//! Reference gate — verify forced-alignment (mtl) line timings against an
//! independent ASR word stream (Gemini 3.5 Transcribe, `g35t_client.rs`)
//! before stamping a song as a ★ reference (issue #143, design settled on
//! #130's 2026-09-12 comment).
//!
//! Matching is a monotonic n-gram anchor search: it borrows the "cursor
//! only ever moves forward" idea from `eval/lyrics/combine_lines_times.py`
//! (that file's `difflib`-based whole-stream alignment is NOT ported here
//! — this is a simpler, purpose-built matcher for a single yes/no gate,
//! not a full line/word recombination), so a repeated chorus line always
//! binds to the NEXT occurrence in the ASR stream, never rebinding
//! backwards onto an earlier line's match.
//!
//! For each line, in order: take its first `min(3, line_len)` normalized
//! words and search the ASR word stream for that exact n-gram starting at
//! the cursor (the word right after the previous line's matched anchor
//! word). If not found, fall back to a shorter n-gram (3 → 2 → 1 words) —
//! this recovers a line whose 3rd word the ASR mis-transcribed while its
//! first word(s) still match. No match at all → the line is unmatched and
//! the cursor does not move.

use crate::lyrics::g35t_client::AsrWord;

/// One forced-alignment (mtl) line under verification. Deliberately
/// separate from `crate::lyrics::backend::AlignedLine` (which carries
/// `u32` ms + optional per-word timing for the production merge path) —
/// this type is the minimal reference-gate input contract.
#[derive(Debug, Clone)]
pub struct AlignedLine {
    pub text: String,
    pub start_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateStats {
    pub lines_total: usize,
    pub lines_matched: usize,
    pub matched_frac: f64,
    pub median_signed_ms: i64,
    pub within_400_frac: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateFailReason {
    Coverage,
    Offset,
    Agreement,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    Pass(GateStats),
    Fail {
        reason: GateFailReason,
        stats: GateStats,
    },
}

pub const MIN_MATCHED_FRAC: f64 = 0.60;
pub const MAX_ABS_MEDIAN_OFFSET_MS: i64 = 400;
pub const MIN_WITHIN_400_FRAC: f64 = 0.70;
pub const WITHIN_MS: i64 = 400;

/// Lowercase + strip everything but letters/digits/apostrophes. Unicode
/// alphanumeric (not ASCII-only) so accented lyrics normalize correctly.
fn normalize_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Whitespace-split + normalize a line's text, dropping any token that
/// normalizes to empty (pure punctuation). `split_whitespace` already
/// collapses runs of whitespace.
fn normalized_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_word)
        .filter(|w| !w.is_empty())
        .collect()
}

/// First position >= `cursor` where `word_norms` contains `ngram`
/// contiguously, or `None`.
fn find_ngram(word_norms: &[String], cursor: usize, ngram: &[String]) -> Option<usize> {
    let n = ngram.len();
    if n == 0 || cursor + n > word_norms.len() {
        return None;
    }
    for start in cursor..=(word_norms.len() - n) {
        if &word_norms[start..start + n] == ngram {
            return Some(start);
        }
    }
    None
}

/// Per-line matched ASR `start_ms`, in the SAME order as `lines`. `None`
/// means no match was found for that line (its normalized word list was
/// empty, or no n-gram fallback found it in the ASR stream).
pub fn match_lines(lines: &[AlignedLine], words: &[AsrWord]) -> Vec<Option<u64>> {
    let word_norms: Vec<String> = words.iter().map(|w| normalize_word(&w.text)).collect();
    let mut cursor: usize = 0;
    let mut out = Vec::with_capacity(lines.len());

    for line in lines {
        let line_words = normalized_words(&line.text);
        if line_words.is_empty() {
            out.push(None);
            continue;
        }

        let max_n = line_words.len().min(3);
        let mut found: Option<usize> = None;
        for n in (1..=max_n).rev() {
            if let Some(pos) = find_ngram(&word_norms, cursor, &line_words[..n]) {
                found = Some(pos);
                break;
            }
        }

        match found {
            Some(pos) => {
                out.push(Some(words[pos].start_ms));
                cursor = pos + 1;
            }
            None => out.push(None),
        }
    }
    out
}

/// Median of an i64 slice (average of the two middle values, rounded, for
/// an even-length slice). `0` for an empty slice — callers only read this
/// field when `lines_matched > 0`, so the empty case never affects a
/// verdict (Coverage already fails first).
fn median_i64(values: &[i64]) -> i64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let a = sorted[n / 2 - 1];
        let b = sorted[n / 2];
        ((a as f64 + b as f64) / 2.0).round() as i64
    }
}

/// Verify `lines` (forced-alignment output) against `words` (independent
/// ASR). Verdict order: Coverage (`matched_frac < 0.60`, incl. zero
/// lines) → Offset (`|median_signed_ms| > 400`) → Agreement
/// (`within_400_frac < 0.70`) → Pass.
pub fn evaluate(lines: &[AlignedLine], words: &[AsrWord]) -> GateVerdict {
    let matches = match_lines(lines, words);

    let mut lines_total = 0usize;
    let mut lines_matched = 0usize;
    let mut deltas: Vec<i64> = Vec::new();
    let mut within_count = 0usize;

    for (line, matched_start) in lines.iter().zip(matches.iter()) {
        if normalized_words(&line.text).is_empty() {
            continue;
        }
        lines_total += 1;
        if let Some(asr_start) = matched_start {
            lines_matched += 1;
            let delta = *asr_start as i64 - line.start_ms as i64;
            deltas.push(delta);
            if delta.abs() <= WITHIN_MS {
                within_count += 1;
            }
        }
    }

    let matched_frac = if lines_total > 0 {
        lines_matched as f64 / lines_total as f64
    } else {
        0.0
    };
    let median_signed_ms = median_i64(&deltas);
    let within_400_frac = if lines_matched > 0 {
        within_count as f64 / lines_matched as f64
    } else {
        0.0
    };

    let stats = GateStats {
        lines_total,
        lines_matched,
        matched_frac,
        median_signed_ms,
        within_400_frac,
    };

    if lines_total == 0 || matched_frac < MIN_MATCHED_FRAC {
        return GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            stats,
        };
    }
    if median_signed_ms.abs() > MAX_ABS_MEDIAN_OFFSET_MS {
        return GateVerdict::Fail {
            reason: GateFailReason::Offset,
            stats,
        };
    }
    if within_400_frac < MIN_WITHIN_400_FRAC {
        return GateVerdict::Fail {
            reason: GateFailReason::Agreement,
            stats,
        };
    }
    GateVerdict::Pass(stats)
}

#[cfg(test)]
mod tests {
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

    /// Push one phrase's words into `words`, one word per 300ms starting
    /// at `start`.
    fn push_phrase(words: &mut Vec<AsrWord>, phrase: &str, start: u64) {
        let mut t = start;
        for tok in phrase.split_whitespace() {
            words.push(word(tok, t, t + 300));
            t += 300;
        }
    }

    #[test]
    fn perfect_agreement_passes_with_full_within_400_frac() {
        let lines = vec![
            line("one two three", 1000),
            line("four five six", 2000),
            line("seven eight nine", 3000),
            line("ten eleven twelve", 4000),
            line("thirteen fourteen fifteen", 5000),
        ];
        let mut words = Vec::new();
        for l in &lines {
            push_phrase(&mut words, &l.text, l.start_ms);
        }

        match evaluate(&lines, &words) {
            GateVerdict::Pass(stats) => {
                assert_eq!(stats.lines_total, 5);
                assert_eq!(stats.lines_matched, 5);
                assert_eq!(stats.matched_frac, 1.0);
                assert_eq!(stats.median_signed_ms, 0);
                assert_eq!(stats.within_400_frac, 1.0);
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn whole_song_22s_shift_fails_offset_despite_full_match() {
        // Forced alignment locked onto the wrong (later) repetition of the
        // whole song — every line's claimed start is 22s later than where
        // the words actually are. Text still matches perfectly (Coverage
        // and Agreement would both pass), but the median offset trips the
        // Offset gate.
        let texts = [
            "alpha beta gamma",
            "delta epsilon zeta",
            "eta theta iota",
            "kappa lambda mu",
            "nu xi omicron",
        ];
        let mut lines = Vec::new();
        let mut words = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            let asr_base = 1000 + i as u64 * 1000;
            lines.push(line(text, asr_base + 22_000));
            push_phrase(&mut words, text, asr_base);
        }

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Offset,
                stats,
            } => {
                assert_eq!(stats.lines_matched, 5);
                assert_eq!(stats.median_signed_ms, -22_000);
            }
            other => panic!("expected Fail(Offset), got {other:?}"),
        }
    }

    #[test]
    fn too_many_unmatched_lines_fails_coverage() {
        // Only 2 of 5 lines' text appears anywhere in the ASR stream —
        // matched_frac = 0.4, well under the 0.60 floor.
        let lines = vec![
            line("apple banana cherry", 1000),
            line("this text is nowhere", 2000),
            line("date elderberry fig", 3000),
            line("also missing entirely", 4000),
            line("also not present here", 5000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "apple banana cherry", 1000);
        push_phrase(&mut words, "date elderberry fig", 3000);

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Coverage,
                stats,
            } => {
                assert_eq!(stats.lines_total, 5);
                assert_eq!(stats.lines_matched, 2);
                assert!((stats.matched_frac - 0.4).abs() < 1e-9);
            }
            other => panic!("expected Fail(Coverage), got {other:?}"),
        }
    }

    #[test]
    fn zero_lines_fails_coverage() {
        let lines: Vec<AlignedLine> = vec![];
        let words: Vec<AsrWord> = vec![];
        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Coverage,
                stats,
            } => {
                assert_eq!(stats.lines_total, 0);
            }
            other => panic!("expected Fail(Coverage), got {other:?}"),
        }
    }

    #[test]
    fn half_of_matched_lines_900ms_off_fails_agreement() {
        // All 4 lines match (Coverage passes); the +900/-900 deltas cancel
        // to a median of 0 (Offset passes), but only 2 of 4 (50%) land
        // within 400ms — under the 70% Agreement floor.
        let lines = vec![
            line("alpha bravo charlie", 1000),
            line("delta echo foxtrot", 5000),
            line("golf hotel india", 9000),
            line("juliet kilo lima", 13000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "alpha bravo charlie", 1900); // +900
        push_phrase(&mut words, "delta echo foxtrot", 4100); // -900
        push_phrase(&mut words, "golf hotel india", 9000); // 0
        push_phrase(&mut words, "juliet kilo lima", 13000); // 0

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Agreement,
                stats,
            } => {
                assert_eq!(stats.lines_matched, 4);
                assert_eq!(stats.median_signed_ms, 0);
                assert!((stats.within_400_frac - 0.5).abs() < 1e-9);
            }
            other => panic!("expected Fail(Agreement), got {other:?}"),
        }
    }

    #[test]
    fn repeated_chorus_lines_match_forward_never_backward() {
        let lines = vec![
            line("we lift you up", 1000),
            line("some unique verse line", 5000),
            line("we lift you up", 9000),
            line("another unique verse", 13000),
            line("we lift you up", 17000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "we lift you up", 1000);
        push_phrase(&mut words, "some unique verse line", 5000);
        push_phrase(&mut words, "we lift you up", 9000);
        push_phrase(&mut words, "another unique verse", 13000);
        push_phrase(&mut words, "we lift you up", 17000);

        let matches = match_lines(&lines, &words);
        let starts: Vec<u64> = matches
            .iter()
            .map(|m| m.expect("every line should match"))
            .collect();
        assert_eq!(starts, vec![1000, 5000, 9000, 13000, 17000]);
        for pair in starts.windows(2) {
            assert!(
                pair[1] > pair[0],
                "cursor must advance monotonically, never rebind backward: {starts:?}"
            );
        }
    }

    /// Real fixture: `gemini-3-5-transcribe_YbGFYaA0SbY.json`'s own
    /// `lines[].words[]` as the independent ASR word source, and that same
    /// file's own `lines[].text`/`start_ms` (optionally shifted) as the
    /// forced-alignment lines under test — a self-consistency check
    /// against real Gemini 3.5 Transcribe output, not synthetic data.
    fn load_fixture_lines_and_words() -> (Vec<AlignedLine>, Vec<AsrWord>) {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../eval/lyrics/reports/2026-09-12-raw/gemini-3-5-transcribe_YbGFYaA0SbY.json"
        ));
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture must be valid JSON");
        let lines_json = v["lines"].as_array().expect("fixture must have lines[]");

        let aligned_lines: Vec<AlignedLine> = lines_json
            .iter()
            .map(|l| AlignedLine {
                text: l["text"].as_str().expect("line.text").to_string(),
                start_ms: l["start_ms"].as_u64().expect("line.start_ms"),
            })
            .collect();

        let words: Vec<AsrWord> = lines_json
            .iter()
            .flat_map(|l| {
                l["words"]
                    .as_array()
                    .expect("line.words")
                    .iter()
                    .map(|w| AsrWord {
                        text: w["text"].as_str().expect("word.text").to_string(),
                        start_ms: w["start_ms"].as_u64().expect("word.start_ms"),
                        end_ms: w["end_ms"].as_u64().expect("word.end_ms"),
                    })
            })
            .collect();

        (aligned_lines, words)
    }

    #[test]
    fn real_fixture_unshifted_passes() {
        let (aligned_lines, words) = load_fixture_lines_and_words();
        match evaluate(&aligned_lines, &words) {
            GateVerdict::Pass(stats) => {
                assert!(stats.matched_frac >= MIN_MATCHED_FRAC);
                assert_eq!(stats.median_signed_ms, 0);
            }
            other => panic!("expected Pass on the unshifted real fixture, got {other:?}"),
        }
    }

    #[test]
    fn real_fixture_shifted_30s_fails_offset() {
        let (mut aligned_lines, words) = load_fixture_lines_and_words();
        for l in &mut aligned_lines {
            l.start_ms += 30_000;
        }
        match evaluate(&aligned_lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Offset,
                stats,
            } => {
                assert_eq!(stats.median_signed_ms, -30_000);
            }
            other => panic!("expected Fail(Offset) on the 30s-shifted fixture, got {other:?}"),
        }
    }
}
