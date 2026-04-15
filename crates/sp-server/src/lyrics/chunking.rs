//! Pure function that plans chunked alignment requests from a `LyricsTrack`.
//!
//! Each non-empty line in the input track contributes at least one
//! `ChunkRequest`. Lines whose word count exceeds `MAX_WORDS_PER_CHUNK`
//! are split into multiple sub-chunks with proportional time slicing so
//! Qwen3-ForcedAligner never receives a chunk long enough to trigger
//! its capacity ceiling (observed on #119 Housefires — one SRT event
//! bundled 32 words into a single 8-second window; the aligner
//! collapsed 27 of them onto the same start_ms).
//!
//! Each chunk's audio window is its proportional slice of the source
//! line's `[start_ms, end_ms]`, padded by ±500 ms (clamped at zero).

use sp_core::lyrics::LyricsTrack;

/// Audio-window pre/post padding applied around each chunk, in milliseconds.
/// 500 ms was validated empirically on #148 Planetshakers "Get This Party
/// Started" — smaller windows trunc'd leading phonemes, larger windows let
/// neighbour-line bleed into the alignment.
pub const CHUNK_PAD_MS: u64 = 500;

/// Maximum words per chunk before splitting. Lines with more words than
/// this get divided into multiple sub-chunks with proportional time
/// slicing. 10 was validated against live YT manual subs across 24 songs
/// on win-resolume — the #148 happy path had max 11 words/line and
/// worked perfectly; #119 Housefires had 30+ word SRT events and
/// collapsed. Splitting at 10 keeps the aligner inside its comfort zone
/// without creating gratuitous sub-chunks for normal-length lines.
pub const MAX_WORDS_PER_CHUNK: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRequest {
    /// Index into the original `LyricsTrack.lines` — assembly uses this
    /// to place aligned words back on their source line.
    pub line_index: usize,
    /// Position within the source line's word stream where this chunk's
    /// words begin. For single-chunk lines this is 0; for split lines
    /// each sub-chunk's `word_offset` identifies where its word slice
    /// belongs relative to the line's full text.
    pub word_offset: usize,
    /// Audio slice start, in ms. Never negative (clamped at 0).
    pub start_ms: u64,
    /// Audio slice end, in ms.
    pub end_ms: u64,
    /// Lyrics text to align against the slice (one full line OR one
    /// sub-phrase when the line was split).
    pub text: String,
    /// Expected word count in this chunk's text.
    pub word_count: usize,
}

/// Build a list of `ChunkRequest`s covering every non-empty line of
/// `track`. Lines with more than `MAX_WORDS_PER_CHUNK` words are split
/// into multiple sub-chunks with proportional audio slicing.
pub fn plan_chunks(track: &LyricsTrack) -> Vec<ChunkRequest> {
    let mut out = Vec::with_capacity(track.lines.len());
    for (idx, line) in track.lines.iter().enumerate() {
        let trimmed = line.en.trim();
        if trimmed.is_empty() {
            continue;
        }
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let total_words = words.len();

        if total_words <= MAX_WORDS_PER_CHUNK {
            out.push(ChunkRequest {
                line_index: idx,
                word_offset: 0,
                start_ms: line.start_ms.saturating_sub(CHUNK_PAD_MS),
                end_ms: line.end_ms.saturating_add(CHUNK_PAD_MS),
                text: trimmed.to_string(),
                word_count: total_words,
            });
            continue;
        }

        // Long line: split into ceil(total_words / MAX_WORDS_PER_CHUNK)
        // sub-chunks. Each sub-chunk gets a proportional audio slice and
        // a `word_offset` telling assembly where its words belong.
        let num_chunks = total_words.div_ceil(MAX_WORDS_PER_CHUNK);
        let base_chunk_size = total_words.div_ceil(num_chunks);
        let line_duration = line.end_ms.saturating_sub(line.start_ms);

        for chunk_i in 0..num_chunks {
            let word_start = chunk_i * base_chunk_size;
            let word_end = ((chunk_i + 1) * base_chunk_size).min(total_words);
            if word_start >= word_end {
                break;
            }

            // Proportional time slice within the original line.
            let ratio_start = word_start as f64 / total_words as f64;
            let ratio_end = word_end as f64 / total_words as f64;
            let sub_start_ms = line.start_ms + (line_duration as f64 * ratio_start).round() as u64;
            let sub_end_ms = line.start_ms + (line_duration as f64 * ratio_end).round() as u64;

            out.push(ChunkRequest {
                line_index: idx,
                word_offset: word_start,
                start_ms: sub_start_ms.saturating_sub(CHUNK_PAD_MS),
                end_ms: sub_end_ms.saturating_add(CHUNK_PAD_MS),
                text: words[word_start..word_end].join(" "),
                word_count: word_end - word_start,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsTrack};

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

    #[test]
    fn plan_chunks_builds_one_request_per_short_line() {
        let t = track(vec![
            line(1000, 3000, "hey there friend"),
            line(4000, 6000, "goodbye"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].line_index, 0);
        assert_eq!(chunks[0].word_offset, 0);
        assert_eq!(chunks[0].start_ms, 500);
        assert_eq!(chunks[0].end_ms, 3500);
        assert_eq!(chunks[0].text, "hey there friend");
        assert_eq!(chunks[0].word_count, 3);

        assert_eq!(chunks[1].line_index, 1);
        assert_eq!(chunks[1].word_offset, 0);
        assert_eq!(chunks[1].start_ms, 3500);
        assert_eq!(chunks[1].end_ms, 6500);
        assert_eq!(chunks[1].word_count, 1);
    }

    #[test]
    fn plan_chunks_clamps_first_line_start_to_zero() {
        let t = track(vec![line(200, 1000, "hello")]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks[0].end_ms, 1500);
    }

    #[test]
    fn plan_chunks_skips_empty_and_whitespace_only_lines() {
        let t = track(vec![
            line(0, 1000, ""),
            line(1000, 2000, "   "),
            line(2000, 3000, "real"),
            line(3000, 4000, "\t\n"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_index, 2);
        assert_eq!(chunks[0].text, "real");
    }

    #[test]
    fn plan_chunks_preserves_line_indices_across_skips() {
        let t = track(vec![
            line(0, 1000, ""),
            line(1000, 2000, "one two"),
            line(2000, 3000, "   "),
            line(3000, 4000, "three"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_index, 1);
        assert_eq!(chunks[1].line_index, 3);
    }

    #[test]
    fn plan_chunks_splits_text_on_any_whitespace_for_word_count() {
        let t = track(vec![line(0, 1000, "hey  there\tfriend\nhello")]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].word_count, 4);
    }

    #[test]
    fn plan_chunks_empty_track_returns_empty_vec() {
        let t = track(vec![]);
        assert_eq!(plan_chunks(&t).len(), 0);
    }

    // -------- Long-line splitting --------

    #[test]
    fn plan_chunks_splits_long_line_into_sub_chunks() {
        // 20-word line over [10_000, 20_000] should split into 2 sub-chunks
        // of 10 words each at [10_000, 15_000] and [15_000, 20_000] (plus pad).
        let long_text: String = (1..=20)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(10_000, 20_000, &long_text)]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].line_index, 0);
        assert_eq!(chunks[0].word_offset, 0);
        assert_eq!(chunks[0].word_count, 10);
        assert_eq!(chunks[0].start_ms, 9_500); // 10_000 - 500 pad
        assert_eq!(chunks[0].end_ms, 15_500); // 15_000 + 500 pad
        assert_eq!(chunks[0].text, "w1 w2 w3 w4 w5 w6 w7 w8 w9 w10");

        assert_eq!(chunks[1].line_index, 0);
        assert_eq!(chunks[1].word_offset, 10);
        assert_eq!(chunks[1].word_count, 10);
        assert_eq!(chunks[1].start_ms, 14_500); // 15_000 - 500 pad
        assert_eq!(chunks[1].end_ms, 20_500); // 20_000 + 500 pad
        assert_eq!(chunks[1].text, "w11 w12 w13 w14 w15 w16 w17 w18 w19 w20");
    }

    #[test]
    fn plan_chunks_splits_32_word_line_into_four_sub_chunks() {
        // Reproduces the #119 Housefires failure mode: 32 words in one
        // SRT event. Must split into 4 sub-chunks with word_offsets
        // 0, 8, 16, 24 (ceil(32/10)=4, ceil(32/4)=8 words per chunk).
        let text: String = (1..=32)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(100_000, 108_000, &text)]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].word_offset, 0);
        assert_eq!(chunks[1].word_offset, 8);
        assert_eq!(chunks[2].word_offset, 16);
        assert_eq!(chunks[3].word_offset, 24);
        // Every sub-chunk's word count matches its text's actual word count.
        for c in &chunks {
            assert_eq!(c.word_count, c.text.split_whitespace().count());
        }
        // All sub-chunks share the same line_index.
        assert!(chunks.iter().all(|c| c.line_index == 0));
        // Total word coverage equals the input word count.
        let total: usize = chunks.iter().map(|c| c.word_count).sum();
        assert_eq!(total, 32);
    }

    #[test]
    fn plan_chunks_boundary_exactly_ten_words_one_chunk() {
        // 10 words must NOT split (boundary of MAX_WORDS_PER_CHUNK).
        let text: String = (1..=10)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(1_000, 11_000, &text)]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1, "10 words must stay as a single chunk");
        assert_eq!(chunks[0].word_offset, 0);
    }

    #[test]
    fn plan_chunks_boundary_eleven_words_splits() {
        // 11 words (just over the boundary) must split into 2 sub-chunks.
        let text: String = (1..=11)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(0, 11_000, &text)]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2, "11 words must split into 2 sub-chunks");
        assert_eq!(chunks[0].word_count + chunks[1].word_count, 11);
    }

    /// Defensive test: a malformed YT SRT event where `start_ms ==
    /// end_ms` (zero-duration line) must not panic. All derived
    /// sub-chunks collapse to the same [start, end] window — the
    /// aligner will return whatever it can, but we survive.
    #[test]
    fn plan_chunks_zero_duration_line_does_not_panic() {
        let t = track(vec![line(5_000, 5_000, "one two three four five")]);
        let chunks = plan_chunks(&t);
        // 5 words < MAX_WORDS_PER_CHUNK → single chunk, zero-duration audio window
        // pre-pad, 500ms pad on each side.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_ms, 4_500);
        assert_eq!(chunks[0].end_ms, 5_500);
        assert_eq!(chunks[0].word_count, 5);
    }

    /// Same invariant for the split-path: a long zero-duration line
    /// produces multiple sub-chunks that all share the same audio
    /// window. Division-by-zero avoided via the `if total_words > MAX`
    /// gate AND because `line_duration as f64 * ratio` is just 0 when
    /// the line has zero length.
    #[test]
    fn plan_chunks_zero_duration_long_line_splits_without_panic() {
        let text: String = (1..=15)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(10_000, 10_000, &text)]);
        let chunks = plan_chunks(&t);
        // 15 words > 10 → must split into 2 sub-chunks
        assert_eq!(chunks.len(), 2);
        // Both sub-chunks share the same [start, end] because duration is 0
        assert_eq!(chunks[0].start_ms, 9_500);
        assert_eq!(chunks[0].end_ms, 10_500);
        assert_eq!(chunks[1].start_ms, 9_500);
        assert_eq!(chunks[1].end_ms, 10_500);
        // word_offsets are still correct so assembly can stitch them back
        assert_eq!(chunks[0].word_offset, 0);
        assert_eq!(chunks[1].word_offset, 8);
    }

    #[test]
    fn plan_chunks_sub_chunk_times_are_proportional_to_word_position() {
        // 20 words over [0, 10_000]: sub-chunk 2 (words 10..20) should
        // span the second half of the audio window [5000, 10_000] plus
        // pad.
        let text: String = (1..=20)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = track(vec![line(0, 10_000, &text)]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);
        // Chunk 0: unpadded would be [0, 5000] → padded [0 clamped, 5500]
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks[0].end_ms, 5_500);
        // Chunk 1: unpadded would be [5000, 10000] → padded [4500, 10500]
        assert_eq!(chunks[1].start_ms, 4_500);
        assert_eq!(chunks[1].end_ms, 10_500);
    }
}
