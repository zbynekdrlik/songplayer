//! Line-length splitter — port of SubtitleEdit's TextSplit.AutoBreak()
//! priority-ordered logic (clean-room reimplementation; we read the
//! algorithm, not the GPL-3.0 source).
//!
//! Default max_chars = 32 (LED wall / ProPresenter style). Configurable.
//! NEVER produces uniform/evenly-distributed output (per
//! `feedback_no_even_distribution.md`).

use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};

pub const DEFAULT_MAX_CHARS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct SplitConfig {
    pub max_chars: usize,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

/// Apply line splitting to every line in the track. Lines under `max_chars`
/// pass through untouched. Lines over are split using the priority order:
/// 1. Sentence-end punctuation (`.!?…`)
/// 2. Comma / pause (`,`, `;`, `:`)
/// 3. Word-boundary balance — find split nearest center
/// 4. Hard fallback — rightmost word boundary ≤ max_chars
pub fn split_track(track: &AlignedTrack, cfg: SplitConfig) -> AlignedTrack {
    let mut out_lines = Vec::with_capacity(track.lines.len());
    for line in &track.lines {
        if line.text.chars().count() <= cfg.max_chars {
            out_lines.push(line.clone());
            continue;
        }
        out_lines.extend(split_line(line, cfg));
    }
    AlignedTrack {
        lines: out_lines,
        provenance: track.provenance.clone(),
        raw_confidence: track.raw_confidence,
    }
}

fn split_line(line: &AlignedLine, cfg: SplitConfig) -> Vec<AlignedLine> {
    let split_idx = find_split_index(&line.text, cfg.max_chars);
    let split_idx = match split_idx {
        Some(i) => i,
        // No safe split found — leave the line alone (better than mid-word break)
        None => return vec![line.clone()],
    };

    let (left_text, right_text) = (
        &line.text[..split_idx].trim_end(),
        &line.text[split_idx..].trim_start(),
    );
    if left_text.is_empty() || right_text.is_empty() {
        return vec![line.clone()];
    }

    // Distribute timing proportional to non-whitespace char counts (NOT uniform).
    // A longer left half gets proportionally more time — content-aware, not
    // evenly distributed (per `feedback_no_even_distribution.md`).
    let total = line
        .text
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
        .max(1);
    let left_chars = left_text.chars().filter(|c| !c.is_whitespace()).count();
    let duration = line.end_ms.saturating_sub(line.start_ms);
    let mid_ms = (line.start_ms + (duration as u64 * left_chars as u64 / total as u64) as u32)
        .min(line.end_ms)
        .max(line.start_ms);

    // Distribute words by their proportional position in the byte string.
    let (left_words, right_words) = split_words_by_index(line, split_idx);

    let left_line = AlignedLine {
        text: left_text.to_string(),
        start_ms: line.start_ms,
        end_ms: mid_ms,
        words: left_words,
    };
    let right_line = AlignedLine {
        text: right_text.to_string(),
        start_ms: mid_ms,
        end_ms: line.end_ms,
        words: right_words,
    };

    // Recursively split halves if still too long
    let mut out = Vec::new();
    if left_line.text.chars().count() > cfg.max_chars {
        out.extend(split_line(&left_line, cfg));
    } else {
        out.push(left_line);
    }
    if right_line.text.chars().count() > cfg.max_chars {
        out.extend(split_line(&right_line, cfg));
    } else {
        out.push(right_line);
    }
    out
}

/// Find the byte-index for the split. Priority order:
/// 1. Sentence-end punctuation rightmost ≤ max_chars
/// 2. Comma rightmost ≤ max_chars
/// 3. Word-boundary nearest the center, constrained to be at or before the
///    max_chars limit (not a global nearest-center search)
/// 4. Rightmost word-boundary ≤ max_chars
fn find_split_index(text: &str, max_chars: usize) -> Option<usize> {
    if text.chars().count() <= max_chars {
        return None;
    }

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let limit_idx = chars.get(max_chars).map(|(i, _)| *i).unwrap_or(text.len());

    // Early return guarantees chars.len() > max_chars, so [..max_chars] is in bounds.

    // 1. Sentence-end (.!?…) rightmost ≤ limit
    for &(i, c) in chars[..max_chars].iter().rev() {
        if matches!(c, '.' | '!' | '?' | '…') {
            // Prefer split AFTER the punctuation
            let next = i + c.len_utf8();
            if next < text.len() {
                return Some(next);
            }
        }
    }

    // 2. Comma / pause rightmost ≤ limit
    for &(i, c) in chars[..max_chars].iter().rev() {
        if matches!(c, ',' | ';' | ':' | '，' | '、') {
            let next = i + c.len_utf8();
            if next < text.len() {
                return Some(next);
            }
        }
    }

    // 3. Word boundary nearest center
    let center = max_chars / 2;
    let center_byte = chars.get(center).map(|(i, _)| *i).unwrap_or(text.len());
    let mut best: Option<(usize, i64)> = None;
    for (idx, c) in text.char_indices() {
        if c == ' ' && idx <= limit_idx {
            let dist = (idx as i64 - center_byte as i64).abs();
            if best.is_none_or(|(_, d)| dist < d) {
                best = Some((idx + 1, dist));
            }
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }

    // 4. Rightmost word boundary ≤ limit
    text[..limit_idx].rfind(' ').map(|i| i + 1)
}

/// Same char-width splitting as `split_track`, but for `sp_core::lyrics::
/// LyricsLine` (line-level, `words: None`). Used by the asr_path flow so its
/// output gets the same ≤`max_chars` LED-wall line breaks the whisperx flow
/// has (per the user: long lines like "His name will bring complete
/// breakthrough" must wrap at ~32 chars). Reuses `find_split_index`; timing is
/// distributed proportional to non-whitespace char count (never uniform, per
/// `feedback_no_even_distribution.md`). The whisperx `AlignedTrack` path is
/// untouched.
pub fn split_lyrics_lines(
    lines: Vec<sp_core::lyrics::LyricsLine>,
    cfg: SplitConfig,
) -> Vec<sp_core::lyrics::LyricsLine> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.en.chars().count() <= cfg.max_chars {
            out.push(line);
        } else {
            out.extend(split_one_lyrics_line(line, cfg));
        }
    }
    out
}

fn split_one_lyrics_line(
    line: sp_core::lyrics::LyricsLine,
    cfg: SplitConfig,
) -> Vec<sp_core::lyrics::LyricsLine> {
    let Some(split_idx) = find_split_index(&line.en, cfg.max_chars) else {
        return vec![line];
    };
    let left_text = line.en[..split_idx].trim_end().to_string();
    let right_text = line.en[split_idx..].trim_start().to_string();
    if left_text.is_empty() || right_text.is_empty() {
        return vec![line];
    }

    let total = line
        .en
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
        .max(1) as u64;
    let left_chars = left_text.chars().filter(|c| !c.is_whitespace()).count() as u64;
    let duration = line.end_ms.saturating_sub(line.start_ms);
    let mid_ms = (line.start_ms + duration * left_chars / total)
        .min(line.end_ms)
        .max(line.start_ms);

    let left = sp_core::lyrics::LyricsLine {
        start_ms: line.start_ms,
        end_ms: mid_ms,
        en: left_text,
        sk: None,
        words: None,
    };
    let right = sp_core::lyrics::LyricsLine {
        start_ms: mid_ms,
        end_ms: line.end_ms,
        en: right_text,
        sk: None,
        words: None,
    };

    let mut out = Vec::new();
    if left.en.chars().count() > cfg.max_chars {
        out.extend(split_one_lyrics_line(left, cfg));
    } else {
        out.push(left);
    }
    if right.en.chars().count() > cfg.max_chars {
        out.extend(split_one_lyrics_line(right, cfg));
    } else {
        out.push(right);
    }
    out
}

fn split_words_by_index(
    line: &AlignedLine,
    byte_idx: usize,
) -> (Option<Vec<AlignedWord>>, Option<Vec<AlignedWord>>) {
    let words = match &line.words {
        Some(w) => w,
        None => return (None, None),
    };
    if words.is_empty() {
        return (None, None);
    }

    // Approximate — words.len() may not equal text.split_whitespace().count() if
    // words came from ASR with different tokenization. Byte-proportional gives a
    // reasonable boundary even on mismatched arrays. `line.text.len()` is the byte
    // length and `byte_idx` is a byte offset, so this is byte-proportional, not
    // char-proportional. For ASCII/Latin text (English/Spanish/Portuguese — mostly
    // 1-2 byte chars) the difference is negligible; it skews toward multibyte
    // regions on CJK text, which is an acceptable approximation for the karaoke
    // use case.
    let split_word = (words.len() * byte_idx / line.text.len().max(1)).min(words.len());
    let (left, right) = words.split_at(split_word);
    (
        if left.is_empty() {
            None
        } else {
            Some(left.to_vec())
        },
        if right.is_empty() {
            None
        } else {
            Some(right.to_vec())
        },
    )
}

#[cfg(test)]
#[path = "line_splitter_tests.rs"]
mod tests;
