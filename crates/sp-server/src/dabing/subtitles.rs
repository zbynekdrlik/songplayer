//! Pure EN/SK subtitle builder for a dubbed video (#182 D3).
//!
//! The Gemini Live Translate session (D4) already returns the EN input
//! transcription and the SK output transcription and saves them as
//! `<base>_dub_transcripts.json`. This module turns that JSON into a normal
//! [`LyricsTrack`] — bilingual, line-level, `words: None` — so the wall and the
//! dashboard render a dub's subtitles exactly like song lyrics, with NO second
//! transcription pass and no extra API cost (the owner's 18.9.2026 verdict).
//!
//! Timing: the SK output transcription arrives as coarse fragments stamped by the
//! chunk-local OUTPUT-audio position (`t_ms`); fragment `i` occupies the window
//! `(t[i-1], t[i]]` (`t[-1] = 0`). Fragments are grouped into lines — a line
//! closes at sentence punctuation, at [`MAX_WORDS_PER_LINE`] words, or on a
//! `> `[`LINE_GAP_MS`] arrival gap — and each line is mapped onto the video
//! timeline through the same placement the mix applied: `video = at_ms +
//! local_ms / tempo`. A legacy JSON (pre-#182) without `at_ms`/`tempo` falls back
//! to `start_ms` and `1.0`. The EN reference line is the slice of the chunk's EN
//! string covering the same cumulative character fraction `[a, b)` that the line
//! covers of the chunk's SK, snapped to word boundaries. Every branch is
//! unit-tested in the sibling `subtitles_tests.rs`.

use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};

/// The `lyrics_source` label stamped on a dub subtitle track.
pub const SOURCE_LIVE_TRANSLATE: &str = "gemini-live-translate";

/// Max words in one subtitle line (the wall reads about two lines).
const MAX_WORDS_PER_LINE: usize = 14;

/// A larger arrival gap than this between consecutive SK fragments closes the
/// line (a real speech pause).
const LINE_GAP_MS: u64 = 1500;

/// The minimum on-screen duration of a subtitle line.
const MIN_LINE_MS: u64 = 400;

/// One SK output-transcription fragment, stamped by the chunk-local output-audio
/// position at the moment it arrived.
#[derive(Debug, Clone, Deserialize)]
pub struct SkFragment {
    /// Chunk-local output-audio position (ms) at arrival — the window END of this
    /// fragment.
    pub t_ms: u64,
    /// The SK text of this fragment (may be empty).
    #[serde(default)]
    pub text: String,
}

/// One chunk of the dub transcripts JSON. Only the fields the subtitle builder
/// needs are deserialized; any others (`index`, `end_ms`, `sk`) are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct DubChunk {
    /// The chunk's source-timeline start — the `at_ms` fallback for a legacy JSON.
    #[serde(default)]
    pub start_ms: u64,
    /// Video-timeline offset where the chunk's output lands. Absent in a legacy
    /// (pre-#182) JSON → `start_ms`.
    #[serde(default)]
    pub at_ms: Option<u64>,
    /// The atempo the mix applied to the chunk. Absent in a legacy JSON → `1.0`.
    #[serde(default)]
    pub tempo: Option<f64>,
    /// The chunk's full EN transcription (one untimed string).
    #[serde(default)]
    pub en: String,
    /// The coarse SK fragments with their chunk-local output positions.
    #[serde(default)]
    pub sk_timed: Vec<SkFragment>,
}

/// The dub transcripts JSON (`<base>_dub_transcripts.json`, written by the D4
/// child). Only the fields the subtitle builder needs are deserialized.
#[derive(Debug, Clone, Deserialize)]
pub struct DubTranscripts {
    /// The per-chunk EN/SK transcriptions, in video-timeline order.
    #[serde(default)]
    pub chunks: Vec<DubChunk>,
}

/// Build a bilingual, line-level [`LyricsTrack`] from the dub transcripts.
///
/// `version` is left `0` here (the persistence layer stamps the real
/// `LYRICS_PIPELINE_VERSION`); the timeline is monotonic across chunks.
pub fn transcripts_to_track(t: &DubTranscripts) -> LyricsTrack {
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut prev_end_ms: u64 = 0;

    for chunk in &t.chunks {
        if chunk.sk_timed.is_empty() {
            continue; // no SK timing → no lines for this chunk
        }
        let at_ms = chunk.at_ms.unwrap_or(chunk.start_ms);
        let tempo = match chunk.tempo {
            Some(x) if x.is_finite() && x > 0.0 => x,
            _ => 1.0,
        };

        // Character length of each SK fragment + its prefix sums, so a line's
        // `[lo, hi]` fragment span maps to a cumulative character fraction of the
        // chunk (used to slice the EN reference).
        let frag_chars: Vec<usize> = chunk
            .sk_timed
            .iter()
            .map(|f| f.text.chars().count())
            .collect();
        let total_sk: usize = frag_chars.iter().sum();

        // EN words with their character lengths (no separators) for the slice.
        let en_words: Vec<&str> = chunk.en.split_whitespace().collect();
        let en_word_chars: Vec<usize> = en_words.iter().map(|w| w.chars().count()).collect();
        let total_en: usize = en_word_chars.iter().sum();

        for (lo, hi) in group_fragments(&chunk.sk_timed) {
            // Chunk-local output window of the line: (t[lo-1], t[hi]].
            let local_start = if lo == 0 {
                0
            } else {
                chunk.sk_timed[lo - 1].t_ms
            };
            let local_end = chunk.sk_timed[hi].t_ms.max(local_start);

            // Video time via the placement the mix applied.
            let mut start_ms = at_ms + (local_start as f64 / tempo).round() as u64;
            let mut end_ms = at_ms + (local_end as f64 / tempo).round() as u64;

            // Monotonic: never start before the previous line ended, and hold each
            // line on screen for at least MIN_LINE_MS.
            if start_ms < prev_end_ms {
                start_ms = prev_end_ms;
            }
            if end_ms < start_ms + MIN_LINE_MS {
                end_ms = start_ms + MIN_LINE_MS;
            }
            prev_end_ms = end_ms;

            // SK line text = the line's fragments joined.
            let sk_text: String = chunk.sk_timed[lo..=hi]
                .iter()
                .map(|f| f.text.as_str())
                .collect::<String>()
                .trim()
                .to_string();

            // EN reference = the words covering the SAME cumulative character
            // fraction [a, b) the line covers of the SK.
            let char_start: usize = frag_chars[..lo].iter().sum();
            let char_end: usize = frag_chars[..=hi].iter().sum();
            let (a, b) = if total_sk == 0 {
                (0.0, 0.0)
            } else {
                (
                    char_start as f64 / total_sk as f64,
                    char_end as f64 / total_sk as f64,
                )
            };
            let en_text = en_slice(&en_words, &en_word_chars, total_en, a, b);

            lines.push(LyricsLine {
                start_ms,
                end_ms,
                en: en_text,
                sk: Some(sk_text),
                words: None,
            });
        }
    }

    LyricsTrack {
        version: 0,
        source: SOURCE_LIVE_TRANSLATE.to_string(),
        language_source: "en".to_string(),
        language_translation: "sk".to_string(),
        lines,
    }
}

/// Group consecutive SK fragments into lines. Closes a line after fragment `i`
/// when its text ends a sentence, the line reaches [`MAX_WORDS_PER_LINE`] words,
/// or the next fragment arrives more than [`LINE_GAP_MS`] later (a pause). The
/// final fragment always closes the last line. Returns inclusive `(lo, hi)`
/// fragment-index ranges.
fn group_fragments(frags: &[SkFragment]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut lo = 0usize;
    let mut words = 0usize;
    for (i, f) in frags.iter().enumerate() {
        words += f.text.split_whitespace().count();
        let ends = ends_sentence(&f.text);
        let hit_word_cap = words >= MAX_WORDS_PER_LINE;
        let gap_next = frags
            .get(i + 1)
            .is_some_and(|n| n.t_ms.saturating_sub(f.t_ms) > LINE_GAP_MS);
        let last = i + 1 == frags.len();
        if ends || hit_word_cap || gap_next || last {
            groups.push((lo, i));
            lo = i + 1;
            words = 0;
        }
    }
    groups
}

/// Whether a fragment's text ends a sentence (`. ! ? …`), ignoring trailing
/// whitespace.
fn ends_sentence(text: &str) -> bool {
    matches!(
        text.trim_end().chars().next_back(),
        Some('.') | Some('!') | Some('?') | Some('…')
    )
}

/// The words of `en_words` whose cumulative character START-fraction falls in
/// `[a, b)`. Deterministic, order-preserving, snapped to word boundaries; every
/// EN word lands in exactly one line because the chunk's line fractions form a
/// contiguous partition of `[0, 1]`. Empty EN → empty string.
fn en_slice(en_words: &[&str], en_word_chars: &[usize], total_en: usize, a: f64, b: f64) -> String {
    if total_en == 0 {
        return String::new();
    }
    let mut selected: Vec<&str> = Vec::new();
    let mut cum = 0usize;
    for (&w, &wc) in en_words.iter().zip(en_word_chars.iter()) {
        let start_frac = cum as f64 / total_en as f64;
        if start_frac >= a && start_frac < b {
            selected.push(w);
        }
        cum += wc;
    }
    selected.join(" ")
}

#[cfg(test)]
#[path = "subtitles_tests.rs"]
mod tests;
