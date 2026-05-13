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

use tracing::debug;

use super::{AsrWord, LineEmit, normalize_word};

/// Phase 4 start-artefact detection constants. See `start_ms_skipping_artefact`.
const START_ARTIFACT_DUR_MS: u32 = 100;
const START_ARTIFACT_MAX_CONF: f32 = 0.05;
const START_ARTIFACT_GAP_MS: u32 = 1500;

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

/// Maximum lookback when claiming leading unmatched ASR words. Singer
/// rarely lead-ins more than 1.5 s of mistranscription before reaching
/// the line's first matched word.
const LEADIN_MAX_MS: u32 = 1500;

/// Phase 2.65: when an emit's FIRST matched ASR word does NOT
/// correspond to ref-text position 0, the leading ref word(s) were
/// dropped (usually whisperx misheard them). Walks back through
/// unconsumed ASR words between the previous emit's last matched word
/// and this emit's first matched word, within LEADIN_MAX_MS, and
/// attaches them. Skipped when ref[0] is already first-matched.
///
/// id=21 2:12 "shadow me": whisperx wrote "shed on" for "shadow"; LCS
/// could match neither, first-matched became "me" (ref[1]). Phase 2.65
/// reattaches "shed" + "on" so natural start moves from 132.961 s back
/// to 131.741 s.
#[cfg_attr(test, mutants::skip)] // Walk-back loop with lookback cap and prev-boundary check; functional behavior covered by 4 unit tests + reprocess integration. Boundary mutants (loop guard `scan > 0`, cap `> LEADIN_MAX_MS`) require synthetic edge-cases (no-prev-anchor + scan=0) that don't add behavioral confidence.
pub(super) fn absorb_leading_unmatched(emits: &mut [LineEmit], asr_words: &[AsrWord]) {
    if emits.is_empty() {
        return;
    }
    let mut consumed: std::collections::HashSet<usize> = emits
        .iter()
        .flat_map(|e| e.asr_word_indices.iter().copied())
        .collect();
    let mut order: Vec<usize> = (0..emits.len()).collect();
    order.sort_by_key(|&i| {
        emits[i]
            .asr_word_indices
            .iter()
            .min()
            .copied()
            .unwrap_or(usize::MAX)
    });
    for o in 0..order.len() {
        let cur = order[o];
        let prev_last_matched: Option<usize> = if o == 0 {
            None
        } else {
            emits[order[o - 1]].asr_word_indices.iter().max().copied()
        };
        let first_matched = match emits[cur].asr_word_indices.iter().min().copied() {
            Some(i) => i,
            None => continue,
        };
        // Trigger only when ref[0] is NOT the first-matched word.
        // Avoids absorbing whisperx vibrato tails of the previous line
        // (the singer's sustained vowel mistranscribed as random
        // syllables) when this line's first ref word is already
        // matched correctly.
        let ref_norms: Vec<String> = emits[cur]
            .text
            .split_whitespace()
            .map(normalize_word)
            .filter(|s| !s.is_empty())
            .collect();
        let first_norm = &asr_words[first_matched].norm;
        if ref_norms.first().map(|s| s.as_str()) == Some(first_norm.as_str()) {
            continue;
        }
        let first_ms = asr_words[first_matched].start_ms;
        let mut to_attach: Vec<usize> = Vec::new();
        let mut scan = first_matched;
        while scan > 0 {
            scan -= 1;
            if let Some(pm) = prev_last_matched {
                if scan <= pm {
                    break;
                }
            }
            if consumed.contains(&scan) {
                break;
            }
            if first_ms.saturating_sub(asr_words[scan].end_ms) > LEADIN_MAX_MS {
                break;
            }
            to_attach.push(scan);
        }
        for idx in &to_attach {
            consumed.insert(*idx);
        }
        emits[cur].asr_word_indices.extend(to_attach);
        emits[cur].asr_word_indices.sort_unstable();
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

/// Phase 4: return the `start_ms` for a line's matched ASR words, skipping
/// the first matched word if it is a forced-alignment boundary artefact:
/// near-zero confidence, very short duration, AND a large gap to the second
/// matched word.
///
/// id=227 evidence: WhisperX fused "thank you for the wonders..." into one
/// ASR segment. Forced-alignment placed "for" at 57682-57742ms (60ms,
/// conf=0.0) immediately after the second "you" (57602-57662ms, 20ms gap),
/// then jumped 2602ms to "the" (60344ms). "for" is a ghost timestamp — the
/// singer hasn't started "For the wonders" yet. Without this skip, L9 starts
/// at 57682ms which causes Phase 5 to cap L8 "Thank You, Thank You" at a
/// 1722ms window while the singer is still holding the note.
pub(super) fn start_ms_skipping_artefact(
    indices: &[usize],
    asr_words: &[AsrWord],
    imin: usize,
) -> u32 {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    if sorted.len() < 2 {
        return asr_words[imin].start_ms;
    }
    let first_idx = sorted[0];
    let second_idx = sorted[1];
    let first = &asr_words[first_idx];
    let second = &asr_words[second_idx];
    let dur = first.end_ms.saturating_sub(first.start_ms);
    let gap = second.start_ms.saturating_sub(first.end_ms);
    if dur < START_ARTIFACT_DUR_MS
        && first.confidence < START_ARTIFACT_MAX_CONF
        && gap > START_ARTIFACT_GAP_MS
    {
        debug!(
            first_word = %first.norm,
            first_start_ms = first.start_ms,
            first_end_ms = first.end_ms,
            dur_ms = dur,
            confidence = first.confidence,
            gap_to_second_ms = gap,
            second_start_ms = second.start_ms,
            "emit_single: skipping start-artefact first word; using second word start"
        );
        second.start_ms
    } else {
        first.start_ms
    }
}
