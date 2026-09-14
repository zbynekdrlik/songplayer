//! Base-tier no-text route (#159): group a Gemini 3.5 Transcribe word stream
//! into singable, LED-wall-width lyric lines.
//!
//! This is the SOLE base tier for songs the v21 mtl reference stage does not
//! ship — no usable text candidate, gate fail, or mtl skip/error. It replaces
//! the deleted WhisperX (`whisperx_replicate`/`Orchestrator`) and asr_path
//! (AssemblyAI U3-Pro) routes with a single Gemini-only path.
//!
//! Pipeline: g35t words → silence-gap grouping (+ coalesce 1–2-word
//! fragments) → line sanitize (monotonic start / min-duration / no-overlap) →
//! LED-wall width split (`line_splitter::split_lyrics_lines`). Line-level only
//! (`words: None`, per the v18 line-timing-only rule).
//!
//! The silence-gap grouping + line sanitizer are salvaged verbatim (re-typed
//! for `g35t_client::AsrWord`) from the deleted `asr_path/{fallback,sanitize}.rs`,
//! whose behaviour was proven against the eval Python
//! `assemblyai_universal_3_pro.py::group_words_into_lines` — the deterministic
//! keep-every-word gap split is model-agnostic, so the same logic serves g35t.

use sp_core::lyrics::{LyricsLine, LyricsTrack};

use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::line_splitter::{SplitConfig, split_lyrics_lines};

/// `lyrics_source` label stamped on rows produced by the g35t base tier.
pub const SOURCE_G35T: &str = "gemini-3-5-transcribe";

/// Build an un-translated line-level `LyricsTrack` from a g35t word stream.
///
/// Returns `None` when the transcript yields no usable lines (blank/empty
/// word stream) — the caller quarantines that row as an ASR gap. The returned
/// track has `sk` unfilled and `language_translation` empty; the worker's
/// shared tail translates + persists it, exactly like the mtl ★ path.
pub fn build_track(words: &[AsrWord], version: u32) -> Option<LyricsTrack> {
    let lines = words_to_lines(words);
    if lines.is_empty() {
        return None;
    }
    Some(LyricsTrack {
        version,
        source: SOURCE_G35T.to_string(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines,
    })
}

/// A new line starts when the silence gap between consecutive words exceeds
/// this. Matches the eval Python `LINE_GAP_MS` (and the deleted asr_path).
const LINE_GAP_MS: u64 = 400;

/// Lines shorter than this (in words) get merged into a neighbour during the
/// coalescing pass — avoids 1–2 word fragments.
const MIN_LINE_WORDS: usize = 3;

/// Don't coalesce a short fragment across a gap longer than this (keeps real
/// section breaks intact — e.g. an instrumental between verses).
const MAX_MERGE_GAP_MS: u64 = 1500;

/// Don't let coalescing produce a line longer than this.
const MAX_LINE_MS: u64 = 9000;

/// Minimum per-line duration; a shorter line has its end clamped up.
const MIN_LINE_DURATION_MS: u64 = 200;

/// Group a Gemini 3.5 Transcribe word stream into LED-wall-ready lyric lines.
/// Returns an empty vec for an empty/blank word stream (the caller quarantines
/// that as an ASR gap).
pub fn words_to_lines(words: &[AsrWord]) -> Vec<LyricsLine> {
    let groups = group_on_silence(words);
    let groups = coalesce_short(groups);
    let lines: Vec<LyricsLine> = groups.iter().map(|g| flush(g)).collect();
    let lines = sanitize_lines(lines);
    split_lyrics_lines(lines, SplitConfig::default())
}

/// Group words into runs separated by silence gaps > LINE_GAP_MS.
fn group_on_silence(words: &[AsrWord]) -> Vec<Vec<&AsrWord>> {
    let mut groups: Vec<Vec<&AsrWord>> = Vec::new();
    let mut current: Vec<&AsrWord> = Vec::new();
    let mut prev_end: Option<u64> = None;
    for w in words {
        if w.text.trim().is_empty() {
            continue;
        }
        if let Some(pe) = prev_end {
            if w.start_ms.saturating_sub(pe) > LINE_GAP_MS && !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
        }
        current.push(w);
        prev_end = Some(w.end_ms);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// Merge groups that are too short (< MIN_LINE_WORDS) into the following group,
/// as long as the gap is small enough and the merged line stays under
/// MAX_LINE_MS. Greedy forward merge.
fn coalesce_short(groups: Vec<Vec<&AsrWord>>) -> Vec<Vec<&AsrWord>> {
    if groups.is_empty() {
        return groups;
    }
    let mut out: Vec<Vec<&AsrWord>> = Vec::new();
    let mut cur = groups[0].clone();
    for g in groups.into_iter().skip(1) {
        let cur_end = cur.last().map(|w| w.end_ms).unwrap_or(0);
        let g_start = g.first().map(|w| w.start_ms).unwrap_or(0);
        let g_end = g.last().map(|w| w.end_ms).unwrap_or(0);
        let cur_start = cur.first().map(|w| w.start_ms).unwrap_or(0);
        let gap = g_start.saturating_sub(cur_end);
        let merged_span = g_end.saturating_sub(cur_start);
        if cur.len() < MIN_LINE_WORDS && gap <= MAX_MERGE_GAP_MS && merged_span <= MAX_LINE_MS {
            cur.extend(g);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = g;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn flush(words: &[&AsrWord]) -> LyricsLine {
    let text = words
        .iter()
        .map(|w| w.text.trim())
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

/// Enforce line-level invariants: monotonic `start_ms` (>= previous end), no
/// overlap, minimum duration. Salvaged from the deleted `asr_path/sanitize.rs`.
fn sanitize_lines(mut lines: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut floor: u64 = 0;
    for line in &mut lines {
        line.start_ms = line.start_ms.max(floor);
        line.end_ms = line.end_ms.max(line.start_ms + MIN_LINE_DURATION_MS);
        floor = line.end_ms;
    }
    lines
}

#[cfg(test)]
#[path = "g35t_transcript_tests.rs"]
mod tests;
