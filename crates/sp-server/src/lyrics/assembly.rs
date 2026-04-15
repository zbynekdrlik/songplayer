//! Pure function that assembles per-chunk aligned word streams back into
//! a `LyricsTrack` with `.words` populated on each line.
//!
//! Input:
//!   - `original`: the line-level `LyricsTrack` the chunks were planned from
//!   - `results`: one `ChunkResult` per `ChunkRequest` produced by Python
//!
//! Output:
//!   - A new `LyricsTrack` where each line whose chunks returned words now
//!     has `.words` populated; lines without any chunk (empty lines that
//!     chunking skipped) keep `.words = None`.
//!
//! Because `chunking::plan_chunks` can split long lines into multiple
//! sub-chunks, assembly iterates through every result and uses its
//! `word_offset` to slot words into the correct position within the
//! source line's full word sequence. Slots not covered by any chunk
//! are padded with placeholder words carrying zero timing so the
//! renderer can detect and skip them; over-aligned sub-chunks have
//! their surplus words dropped.

use sp_core::lyrics::{LyricsTrack, LyricsWord};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub line_index: usize,
    /// Position within the source line's word stream where the first
    /// word of this chunk belongs. Matches the `word_offset` of the
    /// `ChunkRequest` that produced this result.
    pub word_offset: usize,
    pub words: Vec<AlignedWord>,
}

/// Merge per-chunk alignment output back into a full `LyricsTrack`.
///
/// - Lines referenced by at least one `ChunkResult` get their `.words`
///   populated with a vec sized to the line's whitespace-split word count.
/// - Each chunk fills a slice `[word_offset .. word_offset+chunk.words.len())`
///   of the output vec.
/// - Slots not covered by any chunk are filled with placeholder words
///   derived from `LyricsLine.en`, carrying `start_ms == end_ms == 0` so
///   the renderer can detect and skip them.
/// - Aligner-over-alignment past the line end is dropped.
pub fn assemble(mut original: LyricsTrack, results: Vec<ChunkResult>) -> LyricsTrack {
    let mut by_line: HashMap<usize, Vec<ChunkResult>> = HashMap::new();
    for r in results {
        if r.line_index >= original.lines.len() {
            continue;
        }
        by_line.entry(r.line_index).or_default().push(r);
    }

    for (line_idx, mut chunks) in by_line {
        let expected_words: Vec<String> = original.lines[line_idx]
            .en
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if expected_words.is_empty() {
            continue;
        }

        // Seed the output with zero-timed placeholders carrying the
        // expected text so every slot has a value even if no chunk
        // covered it.
        let mut out: Vec<LyricsWord> = expected_words
            .iter()
            .map(|text| LyricsWord {
                text: text.clone(),
                start_ms: 0,
                end_ms: 0,
            })
            .collect();

        // Sort chunks by word_offset so later chunks don't clobber
        // earlier ones if there happens to be an overlap from a
        // mis-aligned split (there shouldn't be — chunks are planned
        // with disjoint offsets — but be defensive).
        chunks.sort_by_key(|c| c.word_offset);

        for chunk in chunks {
            for (i, aw) in chunk.words.iter().enumerate() {
                let slot = chunk.word_offset + i;
                if slot >= out.len() {
                    break; // aligner over-delivered — drop surplus
                }
                out[slot] = LyricsWord {
                    text: aw.text.clone(),
                    start_ms: aw.start_ms,
                    end_ms: aw.end_ms,
                };
            }
        }
        original.lines[line_idx].words = Some(out);
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
    fn assemble_exact_word_count_single_chunk_places_every_word() {
        let orig = track(vec![line(1000, 3000, "hey there friend")]);
        let results = vec![ChunkResult {
            line_index: 0,
            word_offset: 0,
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
    fn assemble_under_aligned_single_chunk_pads_with_zero_timing_placeholders() {
        let orig = track(vec![line(0, 2000, "one two three four")]);
        let results = vec![ChunkResult {
            line_index: 0,
            word_offset: 0,
            words: vec![aw(100, 200, "one"), aw(200, 300, "two")],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].start_ms, 100);
        assert_eq!(words[1].start_ms, 200);
        assert_eq!(words[2].start_ms, 0);
        assert_eq!(words[2].text, "three");
        assert_eq!(words[3].text, "four");
    }

    #[test]
    fn assemble_over_aligned_drops_surplus() {
        let orig = track(vec![line(0, 2000, "one two")]);
        let results = vec![ChunkResult {
            line_index: 0,
            word_offset: 0,
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
            word_offset: 0,
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
            word_offset: 0,
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
            word_offset: 0,
            words: vec![aw(0, 500, "x")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_none());
    }

    // -------- Multi-chunk lines (long-line splitting) --------

    #[test]
    fn assemble_merges_two_sub_chunks_from_one_line() {
        let orig = track(vec![line(0, 6_000, "a b c d e f")]);
        let results = vec![
            ChunkResult {
                line_index: 0,
                word_offset: 0,
                words: vec![
                    aw(100, 500, "a"),
                    aw(600, 1_000, "b"),
                    aw(1_200, 1_700, "c"),
                ],
            },
            ChunkResult {
                line_index: 0,
                word_offset: 3,
                words: vec![
                    aw(3_200, 3_800, "d"),
                    aw(4_100, 4_600, "e"),
                    aw(5_100, 5_700, "f"),
                ],
            },
        ];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 6);
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(words[0].start_ms, 100);
        assert_eq!(words[3].start_ms, 3_200);
        assert_eq!(words[5].start_ms, 5_100);
        assert!(words.iter().all(|w| w.start_ms > 0 || w.end_ms > 0));
    }

    #[test]
    fn assemble_handles_missing_middle_chunk_with_placeholders() {
        let orig = track(vec![line(
            0,
            12_000,
            "w0 w1 w2 w3 w4 w5 w6 w7 w8 w9 w10 w11",
        )]);
        let results = vec![
            ChunkResult {
                line_index: 0,
                word_offset: 0,
                words: vec![
                    aw(100, 200, "w0"),
                    aw(300, 400, "w1"),
                    aw(500, 600, "w2"),
                    aw(700, 800, "w3"),
                ],
            },
            ChunkResult {
                line_index: 0,
                word_offset: 8,
                words: vec![
                    aw(9_000, 9_100, "w8"),
                    aw(9_300, 9_400, "w9"),
                    aw(9_600, 9_700, "w10"),
                    aw(10_000, 10_100, "w11"),
                ],
            },
        ];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 12);
        assert_eq!(words[0].text, "w0");
        assert_eq!(words[0].start_ms, 100);
        for i in 4..8 {
            assert_eq!(words[i].start_ms, 0);
            assert_eq!(words[i].end_ms, 0);
            assert_eq!(words[i].text, format!("w{i}"));
        }
        assert_eq!(words[8].text, "w8");
        assert_eq!(words[8].start_ms, 9_000);
    }

    #[test]
    fn assemble_sub_chunk_over_alignment_truncates_at_line_boundary() {
        let orig = track(vec![line(0, 2_000, "a b c d")]);
        let results = vec![ChunkResult {
            line_index: 0,
            word_offset: 2,
            words: vec![
                aw(1_000, 1_200, "c"),
                aw(1_400, 1_600, "d"),
                aw(1_700, 1_900, "extra"),
            ],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].text, "a");
        assert_eq!(words[0].start_ms, 0);
        assert_eq!(words[2].text, "c");
        assert_eq!(words[2].start_ms, 1_000);
        assert_eq!(words[3].text, "d");
        assert_eq!(words[3].start_ms, 1_400);
    }
}
