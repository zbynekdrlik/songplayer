//! Pure function that plans chunked alignment requests from a `LyricsTrack`.
//!
//! Each line in the input track becomes one `ChunkRequest`. The chunk's
//! audio window is the line's `[start_ms, end_ms]` padded by ±500 ms
//! (clamped to `>= 0`). Word counts are computed from `line.en` by
//! whitespace split so the assembly phase can redistribute aligned words
//! back to their source line.

use sp_core::lyrics::LyricsTrack;

/// Audio-window pre/post padding applied around each line, in milliseconds.
/// 500 ms was validated empirically on #148 Planetshakers "Get This Party
/// Started" — smaller windows trunc'd leading phonemes, larger windows let
/// neighbour-line bleed into the alignment.
pub const CHUNK_PAD_MS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRequest {
    /// Index into the original `LyricsTrack.lines` — assembly uses this
    /// to place aligned words back on their source line.
    pub line_index: usize,
    /// Audio slice start, in ms. Never negative (clamped at 0).
    pub start_ms: u64,
    /// Audio slice end, in ms.
    pub end_ms: u64,
    /// Lyrics text to align against the slice (one line).
    pub text: String,
    /// Expected word count. The aligner may return fewer or more; the
    /// assembly phase handles both cases.
    pub word_count: usize,
}

/// Build a `ChunkRequest` per non-empty line of `track`.
///
/// Empty lines (`.en` trimmed is empty) are skipped. The start/end of
/// each chunk is padded by `CHUNK_PAD_MS` on both sides, clamped to zero
/// on the low end so the first line doesn't produce a negative slice.
pub fn plan_chunks(track: &LyricsTrack) -> Vec<ChunkRequest> {
    let mut out = Vec::with_capacity(track.lines.len());
    for (idx, line) in track.lines.iter().enumerate() {
        let trimmed = line.en.trim();
        if trimmed.is_empty() {
            continue;
        }
        let word_count = trimmed.split_whitespace().count();
        if word_count == 0 {
            continue;
        }
        let start_ms = line.start_ms.saturating_sub(CHUNK_PAD_MS);
        let end_ms = line.end_ms.saturating_add(CHUNK_PAD_MS);
        out.push(ChunkRequest {
            line_index: idx,
            start_ms,
            end_ms,
            text: trimmed.to_string(),
            word_count,
        });
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
    fn plan_chunks_builds_one_request_per_non_empty_line() {
        let t = track(vec![
            line(1000, 3000, "hey there friend"),
            line(4000, 6000, "goodbye"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].line_index, 0);
        assert_eq!(chunks[0].start_ms, 500); // 1000 - 500 pad
        assert_eq!(chunks[0].end_ms, 3500); // 3000 + 500 pad
        assert_eq!(chunks[0].text, "hey there friend");
        assert_eq!(chunks[0].word_count, 3);

        assert_eq!(chunks[1].line_index, 1);
        assert_eq!(chunks[1].start_ms, 3500);
        assert_eq!(chunks[1].end_ms, 6500);
        assert_eq!(chunks[1].word_count, 1);
    }

    #[test]
    fn plan_chunks_clamps_first_line_start_to_zero() {
        let t = track(vec![line(200, 1000, "hello")]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].start_ms, 0,
            "200ms - 500ms pad must clamp to 0 not wrap around"
        );
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
        // Line index must still point at the original slot in track.lines
        // — assembly relies on this to slot words back.
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
}
