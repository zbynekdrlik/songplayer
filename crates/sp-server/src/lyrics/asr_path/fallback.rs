//! Silence-gap line splitter — the primary (and only) line-building step for
//! asr_path. AssemblyAI returns words + timings; this groups them into
//! singable lines on silence gaps, then coalesces short fragments so we don't
//! emit 1-2 word lines (e.g. "I" / "know" as separate lines). Deterministic,
//! keeps every word, real AAI timings.
//!
//! Mirrors `eval/lyrics/backends/assemblyai_universal_3_pro.py::group_words_into_lines`
//! for the base gap split; the coalescing pass is additional.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiWord;
use crate::lyrics::asr_path::sanitize::sanitize_lines;

/// A new line starts when the silence gap between consecutive words exceeds
/// this. Matches the eval Python LINE_GAP_MS.
pub const LINE_GAP_MS: u64 = 400;

/// Lines shorter than this (in words) get merged into a neighbour during the
/// coalescing pass — avoids 1-2 word fragments.
const MIN_LINE_WORDS: usize = 3;

/// Don't coalesce a short fragment across a gap longer than this (keeps real
/// section breaks intact — e.g. an instrumental between verses).
const MAX_MERGE_GAP_MS: u64 = 1500;

/// Don't let coalescing produce a line longer than this.
const MAX_LINE_MS: u64 = 9000;

pub fn split_on_silence(words: &[AaiWord]) -> Vec<LyricsLine> {
    let groups = group_on_silence(words);
    let groups = coalesce_short(groups);
    let lines = groups.iter().map(|g| flush(g)).collect();
    sanitize_lines(lines)
}

/// Group words into runs separated by silence gaps > LINE_GAP_MS.
fn group_on_silence(words: &[AaiWord]) -> Vec<Vec<&AaiWord>> {
    let mut groups: Vec<Vec<&AaiWord>> = Vec::new();
    let mut current: Vec<&AaiWord> = Vec::new();
    let mut prev_end: Option<u64> = None;
    for w in words {
        if w.text.is_empty() {
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
/// MAX_LINE_MS. Greedy forward merge: keep absorbing the next group while the
/// accumulated group is still too short.
fn coalesce_short(groups: Vec<Vec<&AaiWord>>) -> Vec<Vec<&AaiWord>> {
    if groups.is_empty() {
        return groups;
    }
    let mut out: Vec<Vec<&AaiWord>> = Vec::new();
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

fn flush(words: &[&AaiWord]) -> LyricsLine {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> AaiWord {
        AaiWord {
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(split_on_silence(&[]).is_empty());
    }

    // Direct group_on_silence tests — exercise the raw gap split BEFORE the
    // coalescing pass, which would otherwise mask the gap-boundary and
    // condition mutations.

    #[test]
    fn group_exact_gap_does_not_split() {
        // gap == LINE_GAP_MS (400) must NOT split — only strictly greater does.
        // Kills the `>` → `>=` mutation at the gap comparison.
        let words = vec![w("a", 0, 500), w("b", 900, 1400)]; // gap exactly 400
        let groups = group_on_silence(&words);
        assert_eq!(groups.len(), 1, "exact-400ms gap must stay one group");
    }

    #[test]
    fn group_just_over_gap_splits() {
        // gap 401 > 400 → split. Confirms the comparison fires just past the
        // boundary (complements the exact-gap test).
        let words = vec![w("a", 0, 500), w("b", 901, 1400)]; // gap 401
        let groups = group_on_silence(&words);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn group_small_gaps_stay_one_group() {
        // No gap exceeds 400 → exactly one group. Kills the `&&` → `||`
        // mutation in the split condition: with `||`, the always-true
        // `prev_end.is_some()` would force a split at every word.
        let words = vec![w("a", 0, 300), w("b", 350, 600), w("c", 650, 900)];
        let groups = group_on_silence(&words);
        assert_eq!(groups.len(), 1, "small gaps must not split");
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn no_silence_gap_yields_single_line() {
        let words = vec![
            w("hello", 0, 500),
            w("world", 600, 1100),
            w("again", 1150, 1600),
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].en, "hello world again");
        assert!(lines[0].words.is_none());
    }

    #[test]
    fn large_gap_starts_new_line() {
        // Two 3-word phrases separated by a 2s gap → two lines (neither is short).
        let words = vec![
            w("the", 0, 300),
            w("greatest", 400, 900),
            w("name", 1000, 1500),
            w("we", 3500, 3800),
            w("praise", 3900, 4400),
            w("him", 4500, 5000),
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "the greatest name");
        assert_eq!(lines[1].en, "we praise him");
    }

    #[test]
    fn short_fragment_coalesces_into_neighbour() {
        // "I" then "know" separated by a small gap → must NOT become two 1-word
        // lines; coalesced into one line.
        let words = vec![
            w("the", 0, 300),
            w("greatest", 400, 900),
            w("name", 1000, 1500),
            w("I", 2100, 2300),    // gap 600 > 400 → new group, but only 1 word
            w("know", 2900, 3300), // gap 600 > 400 → new group, 1 word
        ];
        let lines = split_on_silence(&words);
        // "I" (1 word) coalesces forward into "know" → "I know"; first line stays.
        assert!(
            lines.iter().all(|l| l.en.split_whitespace().count() >= 2),
            "no 1-word lines: {lines:?}"
        );
    }

    #[test]
    fn does_not_coalesce_across_long_gap() {
        // A short fragment followed by a long instrumental gap stays separate.
        let words = vec![
            w("yeah", 0, 400),
            w("we", 6000, 6300),
            w("praise", 6400, 6900),
            w("him", 7000, 7500),
        ];
        let lines = split_on_silence(&words);
        // gap 5600 > MAX_MERGE_GAP_MS (1500) → "yeah" not merged forward.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "yeah");
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let words = vec![w("a", 0, 500), w("b", 600, 1100), w("c", 1150, 1600)];
        assert!(split_on_silence(&words).iter().all(|l| l.words.is_none()));
    }
}
