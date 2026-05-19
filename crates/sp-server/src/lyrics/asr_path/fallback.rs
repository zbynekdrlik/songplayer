//! Silence-gap line splitter — fallback when Claude rejects the merge.
//!
//! Mirrors `eval/lyrics/backends/assemblyai_universal_3_pro.py::group_words_into_lines`.
//! A new line starts when the gap between the previous word's end and the
//! current word's start exceeds LINE_GAP_MS milliseconds.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiWord;
use crate::lyrics::asr_path::sanitize::sanitize_lines;

/// Match eval Python `LINE_GAP_MS` exactly. Changes here must update the
/// eval Python in lockstep so eval-time and production-time outputs stay
/// comparable when investigating regressions.
pub const LINE_GAP_MS: u64 = 400;

pub fn split_on_silence(words: &[AaiWord]) -> Vec<LyricsLine> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut current: Vec<&AaiWord> = Vec::new();
    let mut prev_end: Option<u64> = None;

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        if let Some(pe) = prev_end {
            if w.start_ms.saturating_sub(pe) > LINE_GAP_MS && !current.is_empty() {
                lines.push(flush(&current));
                current.clear();
            }
        }
        current.push(w);
        prev_end = Some(w.end_ms);
    }
    if !current.is_empty() {
        lines.push(flush(&current));
    }
    sanitize_lines(lines)
}

fn flush(words: &[&AaiWord]) -> LyricsLine {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    LyricsLine {
        start_ms: words[0].start_ms,
        end_ms: words[words.len() - 1].end_ms,
        en: text,
        sk: None,
        words: None, // per feedback_line_timing_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> AaiWord {
        AaiWord {
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let lines = split_on_silence(&[]);
        assert!(lines.is_empty());
    }

    #[test]
    fn no_silence_gap_yields_single_line() {
        let words = vec![w("hello", 0, 500), w("world", 600, 1100)];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].en, "hello world");
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
        assert!(lines[0].words.is_none());
    }

    #[test]
    fn large_gap_starts_new_line() {
        let words = vec![
            w("first", 0, 500),
            w("line", 600, 1100),
            // 1100 + 400 < 1600 → new line (gap = 500 > LINE_GAP_MS=400)
            w("second", 1600, 2100),
            w("line", 2200, 2700),
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "first line");
        assert_eq!(lines[1].en, "second line");
    }

    #[test]
    fn boundary_at_exactly_400ms_does_not_split() {
        // Gap == LINE_GAP_MS doesn't split; only strictly greater does.
        let words = vec![
            w("a", 0, 500),
            w("b", 900, 1400), // gap = 400 exactly
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let words = vec![w("a", 0, 100)];
        let lines = split_on_silence(&words);
        assert!(lines.iter().all(|l| l.words.is_none()));
    }
}
