//! Resolve Claude's word-index ranges to ms-timed LyricsLines.
//!
//! Per spec rule 3 (`feedback_line_timing_only`), every output line ships
//! `words: None`. Per spec rule 2 (v15-prevention), Claude never sees ms
//! values — only word indices — and the resolver computes ms STRICTLY by
//! lookup, never interpolation.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiTranscript;
use crate::lyrics::asr_path::claude_merge::ClaudeMergeResult;

const MIN_LINE_DURATION_MS: u64 = 200;

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("word index {got} out of range (len={len})")]
    OutOfRange { got: usize, len: usize },
    #[error("inverted range: start_word_idx={start} > end_word_idx={end}")]
    InvertedRange { start: usize, end: usize },
    #[error("empty lines from Claude (caller should fall back)")]
    Empty,
}

pub fn resolve(
    merged: &ClaudeMergeResult,
    aai: &AaiTranscript,
) -> Result<Vec<LyricsLine>, ResolverError> {
    if merged.lines.is_empty() {
        return Err(ResolverError::Empty);
    }
    let mut out: Vec<LyricsLine> = Vec::with_capacity(merged.lines.len());
    let word_count = aai.words.len();
    for ml in &merged.lines {
        if ml.start_word_idx > ml.end_word_idx {
            return Err(ResolverError::InvertedRange {
                start: ml.start_word_idx,
                end: ml.end_word_idx,
            });
        }
        if ml.end_word_idx >= word_count {
            return Err(ResolverError::OutOfRange {
                got: ml.end_word_idx,
                len: word_count,
            });
        }
        let start_ms = aai.words[ml.start_word_idx].start_ms;
        let end_ms = aai.words[ml.end_word_idx].end_ms;
        out.push(LyricsLine {
            start_ms,
            end_ms,
            en: ml.text.clone(),
            sk: None,    // translator fills this later as Some(...)
            words: None, // per feedback_line_timing_only — line-only display
        });
    }
    Ok(sanitize_lines(out))
}

/// Line-level sanitizer:
/// - monotonic `start_ms` (each line's start >= previous line's end)
/// - no overlap (line N+1 start clamped up to line N end if necessary)
/// - minimum 200ms duration (very short lines get clamped to start+200)
fn sanitize_lines(mut lines: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut floor: u64 = 0;
    for line in &mut lines {
        if line.start_ms < floor {
            line.start_ms = floor;
        }
        if line.end_ms < line.start_ms + MIN_LINE_DURATION_MS {
            line.end_ms = line.start_ms + MIN_LINE_DURATION_MS;
        }
        floor = line.end_ms;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::asr_path::aai_backend::AaiWord;
    use crate::lyrics::asr_path::claude_merge::MergedLine;

    fn aai(words: Vec<(&str, u64, u64)>) -> AaiTranscript {
        AaiTranscript {
            words: words
                .into_iter()
                .map(|(t, s, e)| AaiWord {
                    text: t.to_string(),
                    start_ms: s,
                    end_ms: e,
                    confidence: 0.9,
                })
                .collect(),
            raw_text: String::new(),
        }
    }

    fn merged(lines: Vec<(&str, usize, usize)>) -> ClaudeMergeResult {
        ClaudeMergeResult {
            disagreement: false,
            notes: String::new(),
            lines: lines
                .into_iter()
                .map(|(t, s, e)| MergedLine {
                    text: t.to_string(),
                    start_word_idx: s,
                    end_word_idx: e,
                })
                .collect(),
        }
    }

    #[test]
    fn happy_path_resolves_lines() {
        let t = aai(vec![
            ("hello", 0, 500),
            ("world", 600, 1100),
            ("again", 1200, 1700),
        ]);
        let m = merged(vec![("Hello world", 0, 1), ("Again", 2, 2)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "Hello world");
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
        assert!(lines[0].words.is_none());
        assert!(lines[0].sk.is_none());
        assert_eq!(lines[1].start_ms, 1200);
        assert_eq!(lines[1].end_ms, 1700);
    }

    #[test]
    fn rejects_inverted_range() {
        let t = aai(vec![("a", 0, 100), ("b", 100, 200)]);
        let m = merged(vec![("bad", 1, 0)]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(
            matches!(err, ResolverError::InvertedRange { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_out_of_range() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![("oops", 0, 5)]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(
            matches!(err, ResolverError::OutOfRange { got: 5, len: 1 }),
            "got {err:?}"
        );
    }

    #[test]
    fn empty_lines_propagates_empty_error() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(matches!(err, ResolverError::Empty));
    }

    #[test]
    fn sanitizer_enforces_monotonic_start() {
        // Pathological: AAI emitted backwards timing for some reason —
        // sanitizer raises later line's start to the previous line's end.
        let t = aai(vec![("x", 1000, 2000), ("y", 500, 800)]);
        let m = merged(vec![("X", 0, 0), ("Y", 1, 1)]);
        let lines = resolve(&m, &t).expect("ok");
        // Second line had start_ms=500 < prev end (2000); sanitizer clamps.
        assert!(lines[1].start_ms >= lines[0].end_ms);
    }

    #[test]
    fn sanitizer_enforces_minimum_duration() {
        let t = aai(vec![("x", 1000, 1050)]); // 50ms — under threshold
        let m = merged(vec![("X", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert!(lines[0].end_ms - lines[0].start_ms >= MIN_LINE_DURATION_MS);
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![("A", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert!(lines.iter().all(|l| l.words.is_none()));
    }
}
