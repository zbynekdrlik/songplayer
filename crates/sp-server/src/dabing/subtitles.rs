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
//! to `start_ms` and `1.0`.
//!
//! EN (#184 H3, paired by overlap since H5): the EN input transcription arrives
//! as fragments stamped on the video timeline (`en_timed`, synchronous with the
//! SK since H4). They are grouped into SENTENCES — at the [`ends_sentence`] rule
//! AND inside a fragment at `. ! ? …` followed by whitespace — and each WHOLE
//! sentence goes to the chunk's line whose CONTENT interval (its own fragments,
//! not the displayed window) it overlaps most, never to an earlier line than the
//! sentence before it. A line may carry zero, one or several EN sentences. A
//! chunk without `en_timed` (a transcript written before H3) has no EN until the
//! video is re-dubbed. Every branch is unit-tested in the sibling
//! `subtitles_tests.rs`.

use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};

/// The `lyrics_source` label stamped on a dub subtitle track.
pub const SOURCE_LIVE_TRANSLATE: &str = "gemini-live-translate";

/// The version of this builder's OUTPUT, stored with every dub subtitle track
/// (#184 H5). The startup backfill rebuilds a stored track with an older version
/// from its saved transcripts JSON, so a pairing change never needs a re-dub.
/// A track stored before the field existed is version 1. Bump it whenever the
/// grouping or the EN pairing output changes.
pub const DUB_SUBTITLES_BUILDER_VERSION: u32 = 2;

/// A fragment's content runs until the next fragment arrives, but never longer
/// than this after its own time (#184 H5 content intervals).
const CONTENT_TAIL_MS: u64 = 1500;

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

/// One EN input-transcription fragment, stamped by its chunk-local arrival
/// (#184 H3): the source position plus the small ASR lag, since the source is
/// streamed at 1.0x wall clock.
#[derive(Debug, Clone, Deserialize)]
pub struct EnFragment {
    /// Chunk-local arrival (ms) of this input-transcription fragment.
    pub t_ms: u64,
    /// The EN text of this fragment (may be empty; carries its own spaces).
    #[serde(default)]
    pub text: String,
}

/// One chunk of the dub transcripts JSON. Only the fields the subtitle builder
/// needs are deserialized; any others (`index`, `end_ms`, `en`, `sk`) are ignored.
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
    /// The EN input-transcription fragments with their chunk-local arrival
    /// times. Absent in a transcript written before #184 H3 → no EN.
    #[serde(default)]
    pub en_timed: Vec<EnFragment>,
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
    // Pass 1 builds each line at its TRUE start (monotonic — never before the
    // previous line's start) and TRUE end; pass 2 (below) derives the displayed
    // end (`max(true_end, start + MIN)` trimmed to the next line's start) so a
    // run of short lines can no longer push every later line later (#182 item 7).
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut prev_start_ms: u64 = 0;

    for chunk in &t.chunks {
        if chunk.sk_timed.is_empty() {
            continue; // no SK timing → no lines for this chunk
        }
        let at_ms = chunk.at_ms.unwrap_or(chunk.start_ms);
        let tempo = match chunk.tempo {
            // Clamp to the supported atempo range (#182 item 8): a tiny tempo
            // would overflow the local_ms/tempo cast + add.
            Some(x) if x.is_finite() && x > 0.0 => x.clamp(0.25, 4.0),
            _ => 1.0,
        };

        // This chunk's lines are `lines[first_line..]`; its EN is assigned to
        // them (and only them) once they are built, by their CONTENT intervals
        // (#184 H5): a line's own fragments `[t(lo), content_end(hi))` on the
        // video timeline — not the displayed window, which opens where the
        // PREVIOUS fragment ended.
        let first_line = lines.len();
        let sk_video: Vec<u64> = chunk
            .sk_timed
            .iter()
            .map(|f| to_video_ms(at_ms, f.t_ms, tempo))
            .collect();
        let mut content: Vec<(u64, u64)> = Vec::new();

        for (lo, hi) in group_fragments(&chunk.sk_timed) {
            // SK line text = the line's fragments joined.
            let sk_text: String = chunk.sk_timed[lo..=hi]
                .iter()
                .map(|f| f.text.as_str())
                .collect::<String>()
                .trim()
                .to_string();
            // A group whose joined SK is empty after trim produces no line
            // (#182 item 9). It does not advance the monotonic start anchor.
            if sk_text.is_empty() {
                continue;
            }

            // Chunk-local output window of the line: (t[lo-1], t[hi]].
            let local_start = if lo == 0 {
                0
            } else {
                chunk.sk_timed[lo - 1].t_ms
            };
            let local_end = chunk.sk_timed[hi].t_ms.max(local_start);

            // Pass 1 timing: the line's TRUE start (monotonic — never before the
            // previous line's START, not its extended end) and TRUE end (never
            // before its own start). The displayed end is finalised in pass 2.
            let start_ms = to_video_ms(at_ms, local_start, tempo).max(prev_start_ms);
            let true_end_ms = to_video_ms(at_ms, local_end, tempo).max(start_ms);
            prev_start_ms = start_ms;

            content.push((sk_video[lo], content_end(&sk_video, hi)));
            lines.push(LyricsLine {
                start_ms,
                // Pass 1 stores the TRUE end here; pass 2 rewrites it.
                end_ms: true_end_ms,
                // Filled below from the chunk's timed EN.
                en: String::new(),
                sk: Some(sk_text),
                words: None,
            });
        }

        // EN (#184 H3/H5): the input fragments on the video timeline through the
        // SAME placement as the SK, then each whole sentence to the line whose
        // content interval it overlaps most.
        let en_video: Vec<EnFragment> = chunk
            .en_timed
            .iter()
            .map(|f| EnFragment {
                t_ms: to_video_ms(at_ms, f.t_ms, tempo),
                text: f.text.clone(),
            })
            .collect();
        let per_line = assign_en_sentences(&en_sentences(&en_video), &content);
        for (line, en) in lines[first_line..].iter_mut().zip(per_line) {
            line.en = en;
        }
    }

    finalize_line_ends(&mut lines);

    LyricsTrack {
        version: 0,
        source: SOURCE_LIVE_TRANSLATE.to_string(),
        language_source: "en".to_string(),
        language_translation: "sk".to_string(),
        lines,
    }
}

/// Pass 2 (#182 item 7): turn each line's TRUE end (currently in `end_ms`) into
/// its DISPLAYED end — `max(true_end, start + MIN_LINE_MS)`, trimmed back to the
/// NEXT line's start when that is earlier so lines never overlap. The last line
/// keeps its untrimmed value. Trimming may push an end below MIN but the end is
/// always kept strictly greater than its own start (if the next line starts at
/// the same time, this line gets `start + 1`).
fn finalize_line_ends(lines: &mut [LyricsLine]) {
    for i in 0..lines.len() {
        let start = lines[i].start_ms;
        let true_end = lines[i].end_ms;
        let mut end = true_end.max(start + MIN_LINE_MS);
        if let Some(next_start) = lines.get(i + 1).map(|n| n.start_ms) {
            // Trim to the next line's start, but never to (or below) this line's
            // own start. `min`/`max` instead of comparisons: `next_start == end`
            // trims to the same value, so a `<` here was an equivalent mutant.
            end = end.min(next_start.max(start + 1));
        }
        lines[i].end_ms = end;
    }
}

/// Chunk-local output position → video-timeline ms through the placement the mix
/// applied (`at_ms + local_ms / tempo`). ONE mapping for a line's start and end.
fn to_video_ms(at_ms: u64, local_ms: u64, tempo: f64) -> u64 {
    // `as u64` saturates a huge/NaN float to u64::MAX (Rust ≥ 1.45), and
    // `saturating_add` caps the offset — a degenerate tempo can never overflow
    // (#182 item 8; tempo is already clamped to 0.25..=4.0 at the call site).
    at_ms.saturating_add((local_ms as f64 / tempo).round() as u64)
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

/// Where the content of fragment `i` ends (#184 H5): at the next fragment's
/// time, but never more than [`CONTENT_TAIL_MS`] after its own; the last
/// fragment's content ends [`CONTENT_TAIL_MS`] after it. `times` are the
/// fragments' video-timeline times, in order.
fn content_end(times: &[u64], i: usize) -> u64 {
    let cap = times[i].saturating_add(CONTENT_TAIL_MS);
    times.get(i + 1).map_or(cap, |&next| next.min(cap))
}

/// One EN sentence and its content interval `[start_ms, end_ms)` on the video
/// timeline (#184 H5): from its first non-blank fragment's time to the
/// [`content_end`] of its last one.
#[derive(Debug, Clone, PartialEq)]
struct EnSentence {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

/// Split a fragment's text after every `. ! ? …` that is followed by whitespace
/// (#184 H5), e.g. `" Good to see you, Nathan. And"` → `[" Good to see you,
/// Nathan.", " And"]`. The pieces keep their own spaces, so they concatenate back
/// to `text`; an empty tail is not returned.
fn split_sentences_inside(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let splits = matches!(c, '.' | '!' | '?' | '…')
            && chars.peek().is_some_and(|&(_, next)| next.is_whitespace());
        if splits {
            let end = i + c.len_utf8();
            pieces.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// The EN fragments grouped into trimmed, non-blank sentences with their content
/// intervals (#184 H5). Fragments are concatenated as-is (Live Translate
/// fragments carry their own spaces, exactly like the SK line text). A sentence
/// closes at a piece ending with [`ends_sentence`] punctuation — a fragment's
/// end, or a split INSIDE a fragment ([`split_sentences_inside`]), where both the
/// part before and the part after belong to that fragment for timing — or at the
/// last fragment. Blank fragments neither start nor end a sentence's interval; a
/// run of blank fragments yields no sentence.
fn en_sentences(en: &[EnFragment]) -> Vec<EnSentence> {
    let times: Vec<u64> = en.iter().map(|f| f.t_ms).collect();
    let mut sentences = Vec::new();
    let mut text = String::new();
    // (first, last) index of the non-blank fragments in the open sentence.
    let mut span: Option<(usize, usize)> = None;
    for (i, f) in en.iter().enumerate() {
        for piece in split_sentences_inside(&f.text) {
            if !piece.trim().is_empty() {
                span = Some((span.map_or(i, |(first, _)| first), i));
            }
            text.push_str(piece);
            if ends_sentence(piece) {
                close_sentence(&mut sentences, &times, &mut text, &mut span);
            }
        }
    }
    close_sentence(&mut sentences, &times, &mut text, &mut span);
    sentences
}

/// Emit the open sentence (when it has a non-blank fragment) and reset the
/// accumulator.
fn close_sentence(
    sentences: &mut Vec<EnSentence>,
    times: &[u64],
    text: &mut String,
    span: &mut Option<(usize, usize)>,
) {
    if let Some((first, last)) = span.take() {
        sentences.push(EnSentence {
            start_ms: times[first],
            end_ms: content_end(times, last),
            text: text.trim().to_string(),
        });
    }
    text.clear();
}

/// Assign each WHOLE EN sentence to a line by CONTENT-interval overlap (#184
/// H5). `lines` are the content intervals `[start, end)` of the chunk's lines,
/// in order. A sentence goes to the line it overlaps most; with no overlap, to
/// the line whose midpoint is nearest to its own; a tie goes to the earlier line.
/// The search starts at the previous sentence's line, so the assignment never
/// goes backwards. Several sentences on one line are joined with a space.
/// Returns one EN string per line (empty when no sentence landed there); with no
/// lines the EN is dropped.
fn assign_en_sentences(sentences: &[EnSentence], lines: &[(u64, u64)]) -> Vec<String> {
    let mut out = vec![String::new(); lines.len()];
    let mut line = 0usize;
    for s in sentences {
        line = best_line_from(lines, line, (s.start_ms, s.end_ms));
        if let Some(slot) = out.get_mut(line) {
            if !slot.is_empty() {
                slot.push(' ');
            }
            slot.push_str(&s.text);
        }
    }
    out
}

/// The line index `>= from` for the interval `span`: the largest positive
/// overlap, else the nearest midpoint; the FIRST index wins a tie (`min_by_key`
/// keeps the first minimum). `from` when there is no line at or after it.
fn best_line_from(lines: &[(u64, u64)], from: usize, span: (u64, u64)) -> usize {
    let most_overlap =
        (from..lines.len()).min_by_key(|&j| std::cmp::Reverse(overlap_ms(lines[j], span)));
    match most_overlap {
        Some(j) if overlap_ms(lines[j], span) > 0 => j,
        // Midpoints compared doubled (`start + end`), so no rounding decides a tie.
        _ => (from..lines.len())
            .min_by_key(|&j| mid2(lines[j]).abs_diff(mid2(span)))
            .unwrap_or(from),
    }
}

/// The length of the intersection of two `[start, end)` intervals (0 if none).
fn overlap_ms(a: (u64, u64), b: (u64, u64)) -> u64 {
    a.1.min(b.1).saturating_sub(a.0.max(b.0))
}

/// Twice an interval's midpoint.
fn mid2(iv: (u64, u64)) -> u64 {
    iv.0.saturating_add(iv.1)
}

#[cfg(test)]
#[path = "subtitles_tests.rs"]
mod tests;
