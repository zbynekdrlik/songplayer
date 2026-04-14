//! Pure function that assembles per-chunk aligned word streams back into
//! a `LyricsTrack` with `.words` populated on each line.
//!
//! Input:
//!   - `original`: the line-level `LyricsTrack` the chunks were planned from
//!   - `results`: one `ChunkResult` per `ChunkRequest` produced by Python
//!
//! Output:
//!   - A new `LyricsTrack` where each line whose chunk returned words now
//!     has `.words` populated; lines without a chunk (empty lines that
//!     chunking skipped) keep `.words = None`.
//!
//! Under-aligned chunks (aligner returned fewer words than expected) leave
//! the remaining words as a synthesised placeholder: text from
//! `LyricsLine.en` split by whitespace, with `start_ms == end_ms == 0` so
//! the renderer can detect and skip them. Over-aligned chunks drop the
//! surplus words.

use sp_core::lyrics::{LyricsTrack, LyricsWord};

#[derive(Debug, Clone)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub line_index: usize,
    pub words: Vec<AlignedWord>,
}

/// Merge per-chunk alignment output back into a full `LyricsTrack`.
///
/// - Lines referenced by a `ChunkResult` get their `.words` populated.
/// - Aligned words beyond the expected count (from `LyricsLine.en` split)
///   are dropped.
/// - Missing aligned words (fewer than expected) are padded with
///   `LyricsWord { start_ms: 0, end_ms: 0, text: "<expected>" }` so the
///   renderer can detect and skip placeholder entries.
pub fn assemble(mut original: LyricsTrack, results: Vec<ChunkResult>) -> LyricsTrack {
    for result in results {
        if result.line_index >= original.lines.len() {
            continue;
        }
        let expected_words: Vec<String> = original.lines[result.line_index]
            .en
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if expected_words.is_empty() {
            continue;
        }

        let mut out = Vec::with_capacity(expected_words.len());
        for (i, expected) in expected_words.iter().enumerate() {
            if let Some(got) = result.words.get(i) {
                out.push(LyricsWord {
                    text: got.text.clone(),
                    start_ms: got.start_ms,
                    end_ms: got.end_ms,
                });
            } else {
                // Aligner under-delivered — synthesize a placeholder with
                // zero timing so renderer skips it.
                out.push(LyricsWord {
                    text: expected.clone(),
                    start_ms: 0,
                    end_ms: 0,
                });
            }
        }
        original.lines[result.line_index].words = Some(out);
    }
    original
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::LyricsLine;

    fn line(start_ms: u64, end_ms: u64, en: &str) -> LyricsLine {
        LyricsLine {
            start_ms,
            end_ms,
            en: en.to_string(),
            sk: None,
            words: None,
        }
    }

    fn track(lines: Vec<LyricsLine>) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "yt_subs".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        }
    }

    fn aw(start_ms: u64, end_ms: u64, text: &str) -> AlignedWord {
        AlignedWord {
            text: text.to_string(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn assemble_exact_word_count_places_every_word() {
        let orig = track(vec![line(1000, 3000, "hey there friend")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![
                aw(1000, 1200, "hey"),
                aw(1200, 1400, "there"),
                aw(1400, 1800, "friend"),
            ],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "hey");
        assert_eq!(words[0].start_ms, 1000);
        assert_eq!(words[2].text, "friend");
        assert_eq!(words[2].start_ms, 1400);
    }

    #[test]
    fn assemble_under_aligned_pads_with_zero_timing_placeholders() {
        let orig = track(vec![line(0, 2000, "one two three four")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(100, 200, "one"), aw(200, 300, "two")],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].start_ms, 100);
        assert_eq!(words[1].start_ms, 200);
        assert_eq!(words[2].start_ms, 0, "missing words get 0 start");
        assert_eq!(words[2].end_ms, 0);
        assert_eq!(words[2].text, "three");
        assert_eq!(words[3].text, "four");
    }

    #[test]
    fn assemble_over_aligned_drops_surplus() {
        let orig = track(vec![line(0, 2000, "one two")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![
                aw(100, 200, "one"),
                aw(200, 300, "two"),
                aw(300, 400, "extra"),
                aw(400, 500, "words"),
            ],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 2);
        assert_eq!(words[1].text, "two");
    }

    #[test]
    fn assemble_leaves_lines_without_results_untouched() {
        let orig = track(vec![
            line(0, 1000, "first line"),
            line(1000, 2000, "untouched line"),
        ]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(0, 500, "first"), aw(500, 1000, "line")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_some());
        assert!(out.lines[1].words.is_none());
    }

    #[test]
    fn assemble_ignores_out_of_bounds_line_index() {
        let orig = track(vec![line(0, 1000, "only line")]);
        let results = vec![ChunkResult {
            line_index: 99,
            words: vec![aw(0, 500, "garbage")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_none());
    }

    #[test]
    fn assemble_empty_line_en_skipped() {
        let orig = track(vec![line(0, 1000, "")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(0, 500, "x")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_none());
    }
}
