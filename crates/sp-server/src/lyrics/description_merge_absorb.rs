//! Phase 2.6 + 2.7 absorption helpers — extend each emit's matched ASR
//! word indices to capture content the upstream matchers (Phase 1 Claude
//! / NW DP, Phase 2 sliding-window) couldn't reach.
//!
//! Phase 2.6 (`absorb_prefix_matches`): walk each emit backward through
//! unconsumed audio. If a contiguous suffix of unconsumed words matches
//! the ref-line text prefix in reverse, attach them. Captures the case
//! where the singer paused mid-phrase longer than Phase 2's
//! sliding-window cap (id=132 3:07 "Your name....[8 s]....is the
//! highest" — window from "your" couldn't reach "is").
//!
//! Phase 2.7 (`absorb_sustained_boundary_tokens`): for each adjacent
//! emit pair, transfer leading same-text-and-close-gap tokens from next
//! to prev. Sustained notes (long "Holyyyy" tokenized as multiple "holy"
//! tokens) at line boundaries all stay with prev so wall doesn't switch
//! mid-sustained-note (id=132 2:55).

use super::{AsrWord, LineEmit, normalize_word};

/// Maximum gap between two same-text ASR tokens to treat as one sustained
/// note (singer holding a vowel). Above this, treat as two separate words.
pub(super) const SUSTAINED_NOTE_MAX_GAP_MS: u32 = 2000;

/// A whisperx token shorter than this is a candidate "artifact" — likely
/// a false split / breath / mistokenization rather than a real sung note.
const ARTIFACT_TOKEN_DUR_MS: u32 = 200;

/// Duration ratio threshold for confirming an artifact: if the next token
/// is at least N× longer than the suspected artifact, treat the short
/// token as noise and let the long token represent the real sung note.
const ARTIFACT_DUR_RATIO: u32 = 5;

pub(super) fn absorb_prefix_matches(emits: &mut [LineEmit], asr_words: &[AsrWord]) {
    let mut consumed: std::collections::HashSet<usize> = emits
        .iter()
        .flat_map(|e| e.asr_word_indices.iter().copied())
        .collect();

    for emit in emits.iter_mut() {
        let ref_norms: Vec<String> = emit
            .text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        if ref_norms.len() <= 1 {
            continue;
        }
        let first_matched = match emit.asr_word_indices.iter().min().copied() {
            Some(i) => i,
            None => continue,
        };
        let first_norm = asr_words[first_matched].norm.clone();
        let first_ref_pos = match ref_norms.iter().position(|r| r == &first_norm) {
            Some(p) => p,
            None => continue,
        };
        if first_ref_pos == 0 {
            continue;
        }

        let mut prefix: Vec<usize> = Vec::new();
        let mut cursor = first_matched;
        'outer: for ref_pos in (0..first_ref_pos).rev() {
            let target = &ref_norms[ref_pos];
            let mut scan = cursor;
            while scan > 0 {
                scan -= 1;
                if consumed.contains(&scan) {
                    break 'outer;
                }
                if &asr_words[scan].norm == target {
                    prefix.push(scan);
                    cursor = scan;
                    continue 'outer;
                }
            }
            break;
        }

        for idx in &prefix {
            consumed.insert(*idx);
            emit.asr_word_indices.push(*idx);
        }
        emit.asr_word_indices.sort_unstable();
    }
}

pub(super) fn absorb_sustained_boundary_tokens(emits: &mut [LineEmit], asr_words: &[AsrWord]) {
    for i in 1..emits.len() {
        let (prev_part, next_part) = emits.split_at_mut(i);
        let prev = prev_part.last_mut().expect("split_at >0");
        let next = &mut next_part[0];

        let next_first_ref_word = next
            .text
            .split_whitespace()
            .next()
            .map(normalize_word)
            .filter(|s| !s.is_empty());

        loop {
            if next.asr_word_indices.len() <= 1 {
                break;
            }
            let prev_last = match prev.asr_word_indices.last() {
                Some(&i) => i,
                None => break,
            };
            let next_first = next.asr_word_indices[0];
            let token_norm = &asr_words[next_first].norm;
            if asr_words[prev_last].norm != *token_norm {
                break;
            }
            let gap = asr_words[next_first]
                .start_ms
                .saturating_sub(asr_words[prev_last].end_ms);
            if gap > SUSTAINED_NOTE_MAX_GAP_MS {
                break;
            }

            // ARTIFACT-REPLACEMENT detection: prev's last token is suspiciously
            // short AND next's first token is much longer. Whisperx tokenized
            // a single sustained note as two — the short one is noise, the
            // long one is the real sung note. Absorb so prev gets the real
            // long note and the line displays through the full sustain.
            // id=132 2:53: prev_last=152 (80 ms) + next_first=153 (2141 ms,
            // 26× longer) — short 152 is artifact, 153 is the real "Holy".
            let prev_dur = asr_words[prev_last]
                .end_ms
                .saturating_sub(asr_words[prev_last].start_ms);
            let next_dur = asr_words[next_first]
                .end_ms
                .saturating_sub(asr_words[next_first].start_ms);
            let is_artifact_replacement = prev_dur < ARTIFACT_TOKEN_DUR_MS
                && next_dur >= prev_dur.saturating_mul(ARTIFACT_DUR_RATIO);

            // When NOT an artifact-replacement: skip absorption if next's
            // first ref word matches the token (it rightfully belongs to
            // next's first sung word). id=132 1:33: 79+80 both 2 s holies —
            // each emit gets its own "Holy" so wall switches at the second
            // holy's start.
            if !is_artifact_replacement
                && next_first_ref_word.as_deref() == Some(token_norm.as_str())
            {
                break;
            }

            prev.asr_word_indices.push(next_first);
            next.asr_word_indices.remove(0);
        }
    }
}
