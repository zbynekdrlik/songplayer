//! Resolve Claude's word-index ranges to ms-timed LyricsLines.
//!
//! Per spec rule 3 (`feedback_line_timing_only`), every output line ships
//! `words: None`. Per spec rule 2 (v15-prevention), Claude never sees ms
//! values — only word indices — and the resolver computes ms STRICTLY by
//! lookup, never interpolation.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiTranscript;
use crate::lyrics::asr_path::claude_merge::ClaudeMergeResult;
use crate::lyrics::asr_path::sanitize::{MIN_LINE_DURATION_MS, sanitize_lines};

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
    // Enforce strictly-increasing, non-overlapping word ranges using the REAL
    // AAI word times. Claude's index assignment is noisy on repeated choruses —
    // it sometimes places a later line at earlier indices (a backward jump).
    // Rather than clamp such a line to the 200ms floor (a blink on the wall),
    // we bump its start past the previous accepted line and, if that leaves no
    // unique words, DROP it. Every surviving line keeps real AAI timing. This
    // is ASR-data-only: no synthesized ms, no interpolation.
    let mut last_end_idx: Option<usize> = None;
    let mut dropped: Vec<String> = Vec::new();
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

        // Bump the start past the previous accepted line so ranges never overlap.
        let effective_start = match last_end_idx {
            Some(pe) if ml.start_word_idx <= pe => pe + 1,
            _ => ml.start_word_idx,
        };
        // If the bump consumed the whole range, this line is fully behind its
        // predecessor (Claude mis-placed it). Drop it — a missing line is far
        // less jarring than a 200ms flash, and the neighbours stay correct.
        if effective_start > ml.end_word_idx {
            dropped.push(format!(
                "[{}..{}] '{}'",
                ml.start_word_idx,
                ml.end_word_idx,
                ml.text.chars().take(40).collect::<String>()
            ));
            continue;
        }

        let start_ms = aai.words[effective_start].start_ms;
        let end_ms = aai.words[ml.end_word_idx].end_ms;
        // A real span shorter than the minimum means Claude crammed a multi-word
        // line onto one or two words (commonly at a repeated chorus or the song
        // tail). Floor-clamping it produces a 200ms blink on the wall; drop it
        // instead. Surviving lines keep genuine AAI durations.
        if end_ms.saturating_sub(start_ms) < MIN_LINE_DURATION_MS {
            dropped.push(format!(
                "[{}..{}] '{}' ({}ms span — squashed)",
                ml.start_word_idx,
                ml.end_word_idx,
                ml.text.chars().take(40).collect::<String>(),
                end_ms.saturating_sub(start_ms)
            ));
            continue;
        }
        last_end_idx = Some(ml.end_word_idx);
        out.push(LyricsLine {
            start_ms,
            end_ms,
            en: ml.text.clone(),
            sk: None,    // translator fills this later as Some(...)
            words: None, // per feedback_line_timing_only — line-only display
        });
    }
    if !dropped.is_empty() {
        tracing::warn!(
            count = dropped.len(),
            detail = %dropped.join(" | "),
            "asr_path resolver: dropped lines Claude mis-placed at backward/overlapping \
             indices (kept neighbours' real timing instead of a 200ms blink)"
        );
    }
    if out.is_empty() {
        return Err(ResolverError::Empty);
    }
    Ok(sanitize_lines(out))
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
    fn squashed_short_line_is_dropped_not_clamped() {
        // A line whose real AAI span is under the minimum (Claude crammed it
        // onto one short word) is DROPPED rather than floor-clamped to a 200ms
        // blink. The long opening line survives; the 50ms tail is dropped.
        let t = aai(vec![
            ("hello", 0, 1500),
            ("world", 1600, 3000),
            ("x", 3001, 3051), // 50ms — squashed
        ]);
        let m = merged(vec![("hello world", 0, 1), ("squashed", 2, 2)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 1, "the 50ms line must be dropped");
        assert_eq!(lines[0].en, "hello world");
        assert!(
            lines
                .iter()
                .all(|l| l.end_ms - l.start_ms >= MIN_LINE_DURATION_MS)
        );
    }

    #[test]
    fn lone_too_short_line_yields_empty() {
        // If the only line is too short, dropping it leaves nothing → Empty;
        // the orchestrator then falls back to the raw AAI silence-gap split.
        let t = aai(vec![("x", 1000, 1050)]); // 50ms
        let m = merged(vec![("X", 0, 0)]);
        let err = resolve(&m, &t).expect_err("must err — sole line dropped");
        assert!(matches!(err, ResolverError::Empty));
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let t = aai(vec![("a", 0, 500)]); // >= MIN so it survives
        let m = merged(vec![("A", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert!(lines.iter().all(|l| l.words.is_none()));
    }

    #[test]
    fn drops_line_mis_placed_fully_behind_previous() {
        // Claude placed a later line at earlier indices (a backward jump). The
        // bump consumes its whole range → it is DROPPED, not clamped to 200ms.
        // The well-placed neighbour keeps its real timing.
        let t = aai(vec![
            ("the", 0, 500),
            ("greatest", 600, 1100),
            ("name", 1200, 1700),
        ]);
        // Line 1 covers 0..2. Line 2 is mis-placed at 0..0 (fully behind).
        let m = merged(vec![("The greatest name", 0, 2), ("misplaced", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 1, "the backward line must be dropped");
        assert_eq!(lines[0].en, "The greatest name");
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1700);
    }

    #[test]
    fn bumps_partial_overlap_start_past_previous() {
        // Line 2 overlaps line 1's tail but has unique words beyond it. Its
        // start is bumped past line 1's end; it keeps the non-overlapping words.
        let t = aai(vec![
            ("a", 0, 500),
            ("b", 600, 1100),
            ("c", 1200, 1700),
            ("d", 1800, 2300),
        ]);
        // Line 1 = 0..2 (a b c). Line 2 = 1..3 (overlaps b,c; unique = d).
        let m = merged(vec![("a b c", 0, 2), ("b c d", 1, 3)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end_ms, 1700); // a..c
        // Line 2 start bumped to idx 3 (d): real start 1800, not the 200ms floor.
        assert_eq!(lines[1].start_ms, 1800);
        assert_eq!(lines[1].end_ms, 2300);
        assert!(lines[1].end_ms - lines[1].start_ms > MIN_LINE_DURATION_MS);
    }

    #[test]
    fn all_lines_dropped_yields_empty_error() {
        // Degenerate: every line after the first is fully behind → all dropped
        // except the first; if even the first can't anchor, Empty. Here lines
        // 2 and 3 are behind line 1, so only line 1 survives (not Empty). To
        // force Empty we'd need zero survivors — covered by empty_lines test.
        let t = aai(vec![("a", 0, 500), ("b", 600, 1100)]);
        let m = merged(vec![("a b", 0, 1), ("behind", 0, 0), ("behind2", 1, 1)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].en, "a b");
    }
}
