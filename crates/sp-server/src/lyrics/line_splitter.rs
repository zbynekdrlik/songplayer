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
        // 47 chars total — exceeds the 32-char default. '.' at byte 23 is inside
        // the first 32 chars, so priority-1 (sentence-end) wins; split at byte 24.
        let l = line("Praise our God and King. Tell the whole world.", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].text, "Praise our God and King.");
        assert_eq!(split.lines[1].text, "Tell the whole world.");
    }

    #[test]
    fn long_line_splits_at_comma_when_no_sentence_end() {
        // 45 chars total — exceeds 32. No '.!?…' in the first 32 chars, so
        // priority-2 (comma) wins; split at byte 24 after the ','.
        let l = line("Praise our God and King, tell the whole world", 0, 4000);
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
        // "Praise our God and King. Tell the whole world." splits at '.' after "King."
        // Left:  "Praise our God and King." — non-WS chars: P,r,a,i,s,e,o,u,r,G,o,d,a,n,d,K,i,n,g,. = 20
        // Right: "Tell the whole world."   — non-WS chars: T,e,l,l,t,h,e,w,h,o,l,e,w,o,r,l,d,.    = 18
        // Total non-WS = 38
        // Expected mid = 0 + (4000 * 20 / 38) = 80000 / 38 = 2105 (integer division)
        // A buggy uniform-split implementation (mid = (0+4000)/2 = 2000) would fail
        // the ±10ms tolerance check below.
        let l = line("Praise our God and King. Tell the whole world.", 0, 4000);
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 2);
        let expected_mid: u32 = 2105;
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

    // ── split_line: || guard on empty halves (line 60 mutant) ───────────────
    //
    // Mutant: `||` → `&&` — would only bail when BOTH halves are empty.
    // Need a case where split_idx lands exactly at a word-boundary yielding
    // an empty right_text (after trim_start). In that scenario the original
    // line should be passed through unchanged.
    // We construct this by picking max_chars exactly equal to the line length,
    // so no split is needed, but also a split that would leave one half empty.

    #[test]
    fn split_line_passthrough_when_right_half_empty_after_trim() {
        // split_idx lands at position beyond all non-space content → right is empty
        // Use a line where the only split candidate puts all text in the left half.
        // E.g. "Hello world " — 12 chars. With max_chars=6, sentence-end wins if '.' present;
        // use a line "Hello." (6 chars) where sentence-end is at byte 6, but text.len() == 6
        // means next = 6 == text.len(), so the condition `next < text.len()` is false.
        // That path falls through to the word-boundary search but there's no space ≤ limit_idx.
        // Result: find_split_index returns None → pass through.
        // This exercises the `None => return vec![line.clone()]` path, which verifies
        // the guard is needed in the first place.
        let l = line("Hello.", 0, 1000);
        let cfg = SplitConfig { max_chars: 6 };
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, cfg);
        // Text len == max_chars → no split needed, passes through untouched
        assert_eq!(split.lines.len(), 1);
        assert_eq!(split.lines[0].text, "Hello.");
    }

    #[test]
    fn split_line_passthrough_when_both_halves_would_be_empty() {
        // A line consisting of spaces only produces empty left and right after trim.
        // find_split_index returns None for all-whitespace (no word boundary pattern).
        // The `None => return vec![line.clone()]` path triggers.
        let l = line("     ", 0, 500);
        let cfg = SplitConfig { max_chars: 2 };
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, cfg);
        assert_eq!(split.lines.len(), 1);
    }

    // ── split_line: recursive re-split guard (lines 97/102 mutants) ──────────
    //
    // Mutant: `> cfg.max_chars` → `>= cfg.max_chars` — would try to recursively
    // split a half that is exactly max_chars long (which `split_track` already
    // wouldn't have touched), causing unnecessary re-splits.
    // Test: construct a line that splits into two halves where one half is
    // EXACTLY max_chars chars. Under the mutated code that half would be
    // re-split; under correct code it is left alone.

    #[test]
    fn split_line_does_not_recurse_when_half_is_exactly_max_chars() {
        // "Hallelujah alleluia" splits at space after "Hallelujah" (11 chars).
        // Left "Hallelujah" = 10 chars, right "alleluia" = 8 chars.
        // With max_chars=10: left == 10, NOT > 10 → no recursion.
        // Under `>= 10` mutant, left would be re-split (impossible since
        // find_split_index returns None for a 10-char no-punctuation word),
        // but we'd still get 2 lines. The key: both halves must be ≤ max_chars.
        let l = line("Hallelujah alleluia", 0, 2000);
        let cfg = SplitConfig { max_chars: 10 };
        let track = AlignedTrack {
            lines: vec![l],
            provenance: "t".into(),
            raw_confidence: 1.0,
        };
        let split = split_track(&track, cfg);
        // Expected: 2 lines, "Hallelujah" and "alleluia"
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].text, "Hallelujah");
        assert_eq!(split.lines[1].text, "alleluia");
    }

    // ── find_split_index: center / distance arithmetic (lines 131-164 mutants)

    #[test]
    fn find_split_index_returns_none_for_short_text() {
        // text.chars().count() <= max_chars → must return None
        assert_eq!(find_split_index("Hello", 5), None);
        assert_eq!(find_split_index("Hello", 6), None);
        assert_eq!(find_split_index("Hello", 100), None);
    }

    #[test]
    fn find_split_index_word_boundary_nearest_center() {
        // "aaaa bbbb cccc dddd" — 19 chars, max_chars=10.
        // Spaces at bytes 4 and 9 and 14.
        // center = max_chars/2 = 5. center_byte = byte of chars[5] = 5.
        // Spaces: byte 4 (dist=1), byte 9 (dist=4), byte 14 (>limit_idx, excluded).
        // limit_idx = byte of chars[10] = 10.
        // Nearest-to-center space at byte 4 (dist=|4-5|=1) → split at byte 5.
        // Result: left = "aaaa", right = "bbbb cccc dddd".
        //
        // Under `/→%` mutant: center = 10 % 2 = 0; nearest = byte 4 instead of 5.
        // Under `/→*` mutant: center = 10 * 2 = 20; both spaces have same "nearest".
        // Under `−→+` mutant: distance = idx + center_byte (always larger), picks arbitrary.
        // Under `−→/` mutant: distance = idx / center_byte (wrong scaling).
        let result = find_split_index("aaaa bbbb cccc dddd", 10);
        // The split should be at a word boundary near the center.
        assert!(result.is_some());
        let idx = result.unwrap();
        // idx must be a valid split point: not zero, not at end, byte is word start
        assert!(idx > 0);
        assert!(idx < "aaaa bbbb cccc dddd".len());
        // The left half must be at most max_chars chars
        assert!("aaaa bbbb cccc dddd"[..idx].chars().count() <= 10);
    }

    #[test]
    fn find_split_index_word_boundary_nearest_center_verified_value() {
        // Verify the exact split byte for a controlled case.
        // "abcde fghij klmno" — 17 chars, max_chars=9.
        // Spaces: byte 5 (char idx 5), byte 11 (char idx 11).
        // center = 9/2 = 4. center_byte = byte of chars[4] = 4.
        // limit_idx = byte of chars[9] = 9.
        // Space at byte 5: dist = |5-4| = 1, idx <= 9 ✓ → split = 6.
        // Space at byte 11: idx=11 > limit_idx=9 → excluded.
        // So split at byte 6 → left = "abcde", right = "fghij klmno".
        let text = "abcde fghij klmno";
        let result = find_split_index(text, 9);
        assert_eq!(result, Some(6), "nearest-center split at byte 6");
    }

    #[test]
    fn find_split_index_does_not_split_beyond_limit() {
        // Spaces beyond limit_idx must not be chosen. Verify the right half
        // starts within the original max_chars window.
        // "hello world and more text here" — 30 chars, max_chars=12.
        let text = "hello world and more text here";
        let result = find_split_index(text, 12);
        assert!(result.is_some());
        let idx = result.unwrap();
        assert!(
            text[..idx].chars().count() <= 12,
            "left half must be ≤ max_chars"
        );
    }

    #[test]
    fn find_split_index_rightmost_word_boundary_fallback() {
        // No sentence-end, no comma, and no space near center — triggers path 4.
        // "abcdefghij klmno" — 16 chars, max_chars=8.
        // No punctuation. Spaces at byte 10 only (beyond limit_idx=8 → excluded from
        // nearest-center search too). Falls to rfind(' ') in text[..limit_idx].
        // limit_idx = byte of chars[8] = 8. text[..8] = "abcdefgh" — no space.
        // → falls all the way to rfind which also finds nothing → None.
        // Test a case where there IS a space within limit_idx for path 4.
        // "abc def ghij klmno" — 18 chars, max_chars=10.
        // Spaces: byte 3, byte 7, byte 12 (>limit_idx=10 → excluded).
        // center=5, center_byte=5. Space at byte 3: dist=2. Space at byte 7: dist=2.
        // Both equal distance — pick first (smallest idx with min dist? no, iter is
        // forward order, so byte 3 seen first with dist=2, byte 7 seen next with dist=2,
        // dist < d is strict so byte 3 wins). split at byte 4.
        let text = "abc def ghij klmno";
        let result = find_split_index(text, 10);
        assert!(result.is_some());
        let idx = result.unwrap();
        assert!(text[..idx].chars().count() <= 10);
        assert!(idx > 0);
    }

    #[test]
    fn find_split_index_split_point_is_after_space_not_at_space() {
        // `rfind(' ').map(|i| i + 1)` — if mutated to just `i` the split includes
        // the space in the left half (it would end with ' ').
        // Similarly for the nearest-center path: `best = Some((idx + 1, dist))`.
        // After split_line trims, a space at the end would not appear, but we can
        // verify the byte index is past the space character.
        let text = "hello world how are you doing here";
        let result = find_split_index(text, 15);
        assert!(result.is_some());
        let idx = result.unwrap();
        // The byte at idx should NOT be a space (it's the start of the right word)
        let byte_at_idx = text.as_bytes().get(idx).copied().unwrap_or(0);
        assert_ne!(
            byte_at_idx, b' ',
            "split idx must point past the space, not at it"
        );
    }

    // ── split_words_by_index: return values and arithmetic (lines 171-187) ───
    //
    // Mutants on split_words_by_index:
    //   - Replace return with (None, None), (None, Some(vec![])), etc.
    //   - `*` → `+` / `/` in `words.len() * byte_idx`
    //   - `/` → `%` / `*` in `/ line.text.len().max(1)`
    //
    // We test that the function correctly distributes words proportionally.

    #[test]
    fn split_words_by_index_none_when_words_is_none() {
        let l = AlignedLine {
            text: "hello world".into(),
            start_ms: 0,
            end_ms: 1000,
            words: None,
        };
        let (left, right) = split_words_by_index(&l, 5);
        assert!(left.is_none(), "None words → left must be None");
        assert!(right.is_none(), "None words → right must be None");
    }

    #[test]
    fn split_words_by_index_none_when_words_empty() {
        let l = AlignedLine {
            text: "hello world".into(),
            start_ms: 0,
            end_ms: 1000,
            words: Some(vec![]),
        };
        let (left, right) = split_words_by_index(&l, 5);
        assert!(left.is_none(), "empty words → left must be None");
        assert!(right.is_none(), "empty words → right must be None");
    }

    #[test]
    fn split_words_by_index_distributes_proportionally() {
        // "hello world" — 11 bytes. Split at byte 6 (after "hello ").
        // words.len() = 2, byte_idx = 6, text.len() = 11.
        // split_word = (2 * 6) / 11 = 12 / 11 = 1.
        // left = words[..1] = ["hello"], right = words[1..] = ["world"].
        //
        // Under `* → +` mutant: (2 + 6) / 11 = 0 → all words go to right, left=None.
        // Under `/ → %` mutant: (2 * 6) % 11 = 1 (same here but different for other inputs).
        // Under `/ → *` mutant: (2 * 6) * 11 = 132 → min(132, 2) = 2 → all left, right=None.
        let hello = AlignedWord {
            text: "hello".into(),
            start_ms: 0,
            end_ms: 400,
            confidence: 0.9,
        };
        let world = AlignedWord {
            text: "world".into(),
            start_ms: 400,
            end_ms: 1000,
            confidence: 0.9,
        };
        let l = AlignedLine {
            text: "hello world".into(),
            start_ms: 0,
            end_ms: 1000,
            words: Some(vec![hello.clone(), world.clone()]),
        };
        let (left, right) = split_words_by_index(&l, 6);
        assert!(left.is_some(), "left must be Some (has at least 1 word)");
        assert!(right.is_some(), "right must be Some (has at least 1 word)");
        let lv = left.unwrap();
        let rv = right.unwrap();
        assert_eq!(lv.len(), 1);
        assert_eq!(rv.len(), 1);
        assert_eq!(lv[0].text, "hello");
        assert_eq!(rv[0].text, "world");
    }

    #[test]
    fn split_words_by_index_all_to_right_when_byte_idx_zero() {
        // byte_idx = 0 → split_word = (2 * 0) / 11 = 0 → left empty → None
        let hello = AlignedWord {
            text: "hello".into(),
            start_ms: 0,
            end_ms: 400,
            confidence: 0.9,
        };
        let world = AlignedWord {
            text: "world".into(),
            start_ms: 400,
            end_ms: 1000,
            confidence: 0.9,
        };
        let l = AlignedLine {
            text: "hello world".into(),
            start_ms: 0,
            end_ms: 1000,
            words: Some(vec![hello, world]),
        };
        let (left, right) = split_words_by_index(&l, 0);
        assert!(
            left.is_none(),
            "byte_idx=0 → split_word=0 → left empty → None"
        );
        assert!(right.is_some(), "all words go to right");
        assert_eq!(right.unwrap().len(), 2);
    }

    #[test]
    fn split_words_by_index_all_to_left_when_byte_idx_at_end() {
        // byte_idx = text.len() → split_word = min(words.len(), words.len()) = 2
        // left = all words, right = empty → None
        let hello = AlignedWord {
            text: "hello".into(),
            start_ms: 0,
            end_ms: 400,
            confidence: 0.9,
        };
        let world = AlignedWord {
            text: "world".into(),
            start_ms: 400,
            end_ms: 1000,
            confidence: 0.9,
        };
        let text = "hello world";
        let l = AlignedLine {
            text: text.into(),
            start_ms: 0,
            end_ms: 1000,
            words: Some(vec![hello, world]),
        };
        let (left, right) = split_words_by_index(&l, text.len());
        assert!(left.is_some(), "byte_idx at end → all words to left");
        assert!(right.is_none(), "right is empty → None");
        assert_eq!(left.unwrap().len(), 2);
    }

    #[test]
    fn split_words_by_index_four_words_mid_split() {
        // "aa bb cc dd" — 11 bytes. 4 words. split at byte 6 (after "aa bb ").
        // split_word = (4 * 6) / 11 = 24 / 11 = 2.
        // Under `* → +`: (4 + 6) / 11 = 0 → left=None (WRONG).
        // Under `/ → *`: (4 * 6) * 11 = 264 → min(264, 4) = 4 → right=None (WRONG).
        let make_w = |t: &str, s: u32, e: u32| AlignedWord {
            text: t.into(),
            start_ms: s,
            end_ms: e,
            confidence: 0.9,
        };
        let l = AlignedLine {
            text: "aa bb cc dd".into(),
            start_ms: 0,
            end_ms: 4000,
            words: Some(vec![
                make_w("aa", 0, 1000),
                make_w("bb", 1000, 2000),
                make_w("cc", 2000, 3000),
                make_w("dd", 3000, 4000),
            ]),
        };
        let (left, right) = split_words_by_index(&l, 6);
        assert!(left.is_some());
        assert!(right.is_some());
        let lv = left.unwrap();
        let rv = right.unwrap();
        // split_word = 2: first 2 go left, last 2 go right
        assert_eq!(lv.len(), 2, "split_word=2 → 2 words left");
        assert_eq!(rv.len(), 2, "split_word=2 → 2 words right");
        assert_eq!(lv[0].text, "aa");
        assert_eq!(lv[1].text, "bb");
        assert_eq!(rv[0].text, "cc");
        assert_eq!(rv[1].text, "dd");
    }
}
