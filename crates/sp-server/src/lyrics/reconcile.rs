//! Anchor-sequence reconciler — keeps WhisperX timing, replaces mishearings
//! with authoritative text from Tier-1 sources.
//!
//! Pattern from karaoke-gen LyricsCorrector:
//! 1. Tokenize WhisperX output and authoritative text into normalized words
//! 2. Compute LCS anchor pairs
//! 3. Walk anchor-bounded gaps, replace WhisperX words with authoritative
//!    words while keeping the timestamp range
//!
//! Replaces text_merge.rs (Claude reconciliation) with deterministic Rust.
//!
//! **No-synthesize contract:** Per `feedback_no_even_distribution.md`, this
//! reconciler NEVER synthesizes evenly-distributed word timings. If any ASR
//! line lacks per-word timing (`words: None`), we return the ASR unchanged
//! rather than inventing approximate slot timings from the line span.

use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::lcs::{lcs_pairs, norm};
use crate::lyrics::tier1::CandidateText;

/// Reconcile a WhisperX-produced AlignedTrack against an authoritative text
/// (concatenation of all CandidateText lines from Tier-1 text-only sources).
///
/// Returns a NEW AlignedTrack: each line's text is the authoritative version
/// in the matching anchor range, but timing comes from WhisperX.
///
/// Graceful degradation — returns `asr.clone()` when:
/// - `authoritative` is empty or yields no non-empty lines
/// - LCS produces no anchor pairs
/// - Any ASR line has `words: None` (see module-level no-synthesize contract)
pub fn reconcile(asr: &AlignedTrack, authoritative: &[CandidateText]) -> AlignedTrack {
    // Early exit: if any ASR line lacks per-word timing, return unchanged.
    // Synthesizing even-distribution word timings for line-only ASR is banned
    // per feedback_no_even_distribution.md; returning the ASR is safer.
    if asr.lines.iter().any(|l| l.words.is_none()) {
        return asr.clone();
    }

    // 1. Flatten authoritative lines into a single word stream + line boundaries
    let auth_text = best_authoritative(authoritative);
    if auth_text.is_empty() {
        return asr.clone();
    }
    let auth_words: Vec<String> = auth_text
        .iter()
        .flat_map(|line| line.split_whitespace().map(|w| norm(w)))
        .collect();
    if auth_words.is_empty() {
        return asr.clone();
    }

    // Build authoritative word list with which-line-it-belongs-to
    let mut auth_word_to_line: Vec<usize> = Vec::with_capacity(auth_words.len());
    for (line_idx, line) in auth_text.iter().enumerate() {
        for _ in line.split_whitespace() {
            auth_word_to_line.push(line_idx);
        }
    }

    // 2. Flatten ASR words across all lines for LCS.
    // At this point every line is guaranteed to have Some(words) from the
    // early-exit check above, so the None branch is unreachable.
    let mut asr_words: Vec<String> = Vec::new();
    let mut asr_word_origin: Vec<(usize, usize)> = Vec::new(); // (line_idx, word_idx_in_line)
    for (li, line) in asr.lines.iter().enumerate() {
        // Safety: early-exit above ensures words.is_some() for every line.
        let words = line
            .words
            .as_ref()
            .expect("early-exit guarantees Some(words)");
        for (wi, w) in words.iter().enumerate() {
            asr_words.push(norm(&w.text));
            asr_word_origin.push((li, wi));
        }
    }
    if asr_words.is_empty() {
        return asr.clone();
    }

    let pairs = lcs_pairs(&asr_words, &auth_words);
    if pairs.is_empty() {
        return asr.clone();
    }

    // 3. Walk anchor pairs, build NEW AlignedTrack with authoritative text
    //    grouped per authoritative line, timing from WhisperX.
    let mut new_lines: Vec<AlignedLine> = Vec::new();
    let mut current_auth_line: Option<usize> = None;
    let mut current_start_ms: u32 = 0;
    let mut current_end_ms: u32 = 0;
    let mut current_words: Vec<AlignedWord> = Vec::new();

    for &(asr_idx, auth_idx) in &pairs {
        let auth_line_idx = auth_word_to_line[auth_idx];
        let (asr_line_idx, asr_word_idx) = asr_word_origin[asr_idx];

        // Get the timing of this anchor word from the ASR per-word array.
        let (start_ms, end_ms, conf) = anchor_timing(
            asr.lines[asr_line_idx]
                .words
                .as_ref()
                .expect("early-exit guarantees Some(words)"),
            asr_word_idx,
        );

        let local_idx = auth_idx - first_word_offset(&auth_text, auth_line_idx);
        let auth_word = auth_text[auth_line_idx]
            .split_whitespace()
            .nth(local_idx)
            .unwrap_or("")
            .to_string();

        if Some(auth_line_idx) != current_auth_line {
            // Flush previous line
            if let Some(prev_li) = current_auth_line {
                new_lines.push(AlignedLine {
                    text: auth_text[prev_li].clone(),
                    start_ms: current_start_ms,
                    end_ms: current_end_ms,
                    words: if current_words.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut current_words))
                    },
                });
            }
            current_auth_line = Some(auth_line_idx);
            current_start_ms = start_ms;
            current_words.clear();
        }
        current_end_ms = end_ms;
        current_words.push(AlignedWord {
            text: auth_word,
            start_ms,
            end_ms,
            confidence: conf,
        });
    }

    // Flush final line
    if let Some(li) = current_auth_line {
        new_lines.push(AlignedLine {
            text: auth_text[li].clone(),
            start_ms: current_start_ms,
            end_ms: current_end_ms,
            words: if current_words.is_empty() {
                None
            } else {
                Some(current_words)
            },
        });
    }

    AlignedTrack {
        lines: new_lines,
        provenance: format!("{}+reconciled", asr.provenance),
        raw_confidence: asr.raw_confidence,
    }
}

/// Look up the timing for a word at `word_idx` in the per-word slice.
/// Takes the words slice directly (caller ensures it is non-empty and
/// that `word_idx` is in-bounds for all anchors produced by `lcs_pairs`).
fn anchor_timing(words: &[AlignedWord], word_idx: usize) -> (u32, u32, f32) {
    if let Some(w) = words.get(word_idx) {
        return (w.start_ms, w.end_ms, w.confidence);
    }
    // word_idx out-of-bounds — should not happen given the LCS build above,
    // but return a zeroed sentinel rather than panicking.
    (0, 0, 0.0)
}

/// Return the flat word offset of the first word in `auth_text[line_idx]`.
fn first_word_offset(auth_text: &[String], line_idx: usize) -> usize {
    auth_text
        .iter()
        .take(line_idx)
        .map(|l| l.split_whitespace().count())
        .sum()
}

/// Pick the strongest authoritative source: prefer one with most lines.
fn best_authoritative(candidates: &[CandidateText]) -> Vec<String> {
    candidates
        .iter()
        .max_by_key(|c| c.lines.len())
        .map(|c| c.lines.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_asr(lines: &[(&str, u32, u32, &[(&str, u32, u32)])]) -> AlignedTrack {
        AlignedTrack {
            lines: lines
                .iter()
                .map(|(text, s, e, words)| AlignedLine {
                    text: text.to_string(),
                    start_ms: *s,
                    end_ms: *e,
                    words: Some(
                        words
                            .iter()
                            .map(|(w, ws, we)| AlignedWord {
                                text: w.to_string(),
                                start_ms: *ws,
                                end_ms: *we,
                                confidence: 0.9,
                            })
                            .collect(),
                    ),
                })
                .collect(),
            provenance: "test@rev1".into(),
            raw_confidence: 0.9,
        }
    }

    #[test]
    fn reconciler_replaces_misheard_word_keeps_timing() {
        // ASR mishears "I've" as "I"; "got", "a", "God" are anchor matches.
        let asr = make_asr(&[(
            "I got a God",
            1000,
            2000,
            &[
                ("I", 1000, 1200),
                ("got", 1200, 1500),
                ("a", 1500, 1700),
                ("God", 1700, 2000),
            ],
        )]);
        let auth = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: vec!["I've got a God".into()],
            line_timings: None,
            has_timing: false,
        }];
        let reconciled = reconcile(&asr, &auth);
        assert_eq!(reconciled.lines.len(), 1);
        assert_eq!(reconciled.lines[0].text, "I've got a God"); // authoritative text
        assert_eq!(reconciled.lines[0].start_ms, 1200); // timing from "got" anchor
        assert!(reconciled.provenance.ends_with("+reconciled"));
    }

    #[test]
    fn reconciler_returns_input_when_no_authoritative_text() {
        let asr = make_asr(&[(
            "Hello world",
            0,
            1000,
            &[("Hello", 0, 500), ("world", 500, 1000)],
        )]);
        let r = reconcile(&asr, &[]);
        assert_eq!(r.lines, asr.lines);
    }

    #[test]
    fn reconciler_returns_input_when_no_lcs_anchors() {
        // ASR and auth share no normalized words → no LCS anchors → return unchanged.
        let asr = make_asr(&[(
            "foo bar baz",
            0,
            1000,
            &[("foo", 0, 333), ("bar", 333, 666), ("baz", 666, 1000)],
        )]);
        let auth = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: vec!["completely different lyrics here".into()],
            line_timings: None,
            has_timing: false,
        }];
        let r = reconcile(&asr, &auth);
        // No anchor matches → return ASR unchanged
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "foo bar baz");
    }

    #[test]
    fn reconciler_returns_input_when_any_line_has_no_word_timing() {
        // Per feedback_no_even_distribution.md: if any line lacks per-word
        // timing, return the ASR unchanged rather than synthesizing timings.
        let asr = AlignedTrack {
            lines: vec![
                AlignedLine {
                    text: "line with words".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    words: Some(vec![
                        AlignedWord {
                            text: "line".into(),
                            start_ms: 0,
                            end_ms: 300,
                            confidence: 0.9,
                        },
                        AlignedWord {
                            text: "with".into(),
                            start_ms: 300,
                            end_ms: 600,
                            confidence: 0.9,
                        },
                        AlignedWord {
                            text: "words".into(),
                            start_ms: 600,
                            end_ms: 1000,
                            confidence: 0.9,
                        },
                    ]),
                },
                AlignedLine {
                    text: "line without words".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    words: None, // segment-only line
                },
            ],
            provenance: "test@rev1".into(),
            raw_confidence: 0.8,
        };
        let auth = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: vec!["line with words".into(), "line without words".into()],
            line_timings: None,
            has_timing: false,
        }];
        let r = reconcile(&asr, &auth);
        // Must return unchanged — no synthesized timings allowed
        assert_eq!(r.lines, asr.lines);
        assert_eq!(r.provenance, "test@rev1");
    }
}
