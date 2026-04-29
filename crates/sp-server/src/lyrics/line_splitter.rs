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
            if best.map_or(true, |(_, d)| dist < d) {
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
mod tests {
    use super::*;

    fn line(text: &str, start_ms: u32, end_ms: u32) -> AlignedLine {
        AlignedLine {
            text: text.into(),
            start_ms,
            end_ms,
            words: None,
        }
    }

    #[test]
    fn default_max_chars_is_32() {
        assert_eq!(DEFAULT_MAX_CHARS, 32);
    }

    #[test]
    fn line_under_max_passes_through() {
        let l = line("Short line.", 0, 1000);
        let track = AlignedTrack {
            lines: vec![l.clone()],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 1);
        assert_eq!(split.lines[0].text, "Short line.");
    }

    #[test]
    fn long_line_splits_at_sentence_end() {
        let l = line("Praise the Lord. Tell the world.", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].text, "Praise the Lord.");
        assert_eq!(split.lines[1].text, "Tell the world.");
    }

    #[test]
    fn long_line_splits_at_comma_when_no_sentence_end() {
        let l = line("Praise the Lord, tell the world", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert!(split.lines.len() >= 2);
        assert!(split.lines[0].text.ends_with(','));
    }

    #[test]
    fn long_line_falls_back_to_word_boundary() {
        let l = line("Hallelujah praise hallelujah praise the Lord", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert!(split.lines.len() >= 2);
        // No mid-word breaks
        for sl in &split.lines {
            assert!(!sl.text.starts_with(' '));
            assert!(!sl.text.ends_with(' '));
        }
    }

    #[test]
    fn timing_proportionally_distributed() {
        // "Praise the Lord. Tell the world." splits at the '.' after "Lord."
        // Left:  "Praise the Lord." — non-WS chars: P,r,a,i,s,e,t,h,e,L,o,r,d,. = 14
        // Right: "Tell the world."  — non-WS chars: T,e,l,l,t,h,e,w,o,r,l,d,.   = 13
        // Total non-WS = 27
        // Expected mid = 0 + (4000 * 14 / 27) = 56000 / 27 = 2074 (integer division)
        // A buggy uniform-split implementation (mid = (0+4000)/2 = 2000) would fail
        // the ±10ms tolerance check below.
        let l = line("Praise the Lord. Tell the world.", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 2);
        let expected_mid: u32 = 2074;
        let actual_mid = split.lines[0].end_ms;
        assert!(
            actual_mid.abs_diff(expected_mid) <= 10,
            "expected mid ≈ {expected_mid}ms (char-weighted), got {actual_mid}ms"
        );
        // Continuity: line 1 end == line 2 start
        assert_eq!(split.lines[0].end_ms, split.lines[1].start_ms);
        // Outer bounds preserved
        assert_eq!(split.lines[0].start_ms, 0);
        assert_eq!(split.lines[1].end_ms, 4000);
    }

    #[test]
    fn timing_proportional_diverges_from_uniform() {
        // Unbalanced split: "Hi." is tiny vs the long tail.
        // Sentence-end priority finds '.' after "Hi" at char 3 (within max_chars=32).
        // Left:  "Hi." — non-WS chars: H,i,. = 3
        // Right: "Then a much longer phrase that runs on for a while."
        //        non-WS: T,h,e,n=4 + a=1 + m,u,c,h=4 + l,o,n,g,e,r=6 + p,h,r,a,s,e=6
        //               + t,h,a,t=4 + r,u,n,s=4 + o,n=2 + f,o,r=3 + a=1 + w,h,i,l,e,.=6 = 41
        // Total non-WS = 3 + 41 = 44
        // Expected mid = 0 + (4000 * 3 / 44) = 12000 / 44 = 272ms
        // Uniform would place mid at (0 + 4000) / 2 = 2000ms — VERY different.
        let l = line(
            "Hi. Then a much longer phrase that runs on for a while.",
            0,
            4000,
        );
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert!(split.lines.len() >= 2, "expected a split");
        // Proportional mid is ≈272ms — far below the uniform 2000ms midpoint.
        // A uniform-split implementation would fail this assertion.
        assert!(
            split.lines[0].end_ms < 600,
            "expected proportional mid < 600ms for tiny left half, got {}ms (uniform would be 2000ms)",
            split.lines[0].end_ms
        );
    }

    #[test]
    fn zero_duration_line_does_not_panic() {
        // Edge case: start_ms == end_ms (zero-duration). The clamp + saturating_sub
        // must produce a finite result without underflow or div-by-zero.
        let l = line("Praise the Lord. Tell the world.", 5000, 5000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert!(!split.lines.is_empty());
        for sl in &split.lines {
            assert_eq!(sl.start_ms, 5000);
            assert_eq!(sl.end_ms, 5000);
        }
    }

    #[test]
    fn no_safe_split_passes_through_long_line() {
        // Single long word (no spaces) — can't split safely
        let l = line(&"a".repeat(50), 0, 1000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 1, "no safe split → preserve original");
    }
}
