//! Line-length splitter — port of SubtitleEdit's TextSplit.AutoBreak()
//! priority-ordered logic (clean-room reimplementation; we read the
//! algorithm, not the GPL-3.0 source).
//!
//! Default max_chars = 32 (LED wall / ProPresenter style). Configurable.
//! NEVER produces uniform/evenly-distributed output (per
//! `feedback_no_even_distribution.md`).

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

/// Char-width splitting for `sp_core::lyrics::LyricsLine` (line-level,
/// `words: None`). Used by the v22 g35t base tier (`g35t_transcript`) so its
/// output gets ≤`max_chars` LED-wall line breaks (per the user: long lines
/// like "His name will bring complete breakthrough" must wrap at ~32 chars).
/// Reuses `find_split_index`; timing is distributed proportional to
/// non-whitespace char count (never uniform, per
/// `feedback_no_even_distribution.md`).
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

#[cfg(test)]
#[path = "line_splitter_tests.rs"]
mod tests;
