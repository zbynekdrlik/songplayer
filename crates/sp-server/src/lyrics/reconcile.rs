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

    // Precompute prefix-sum: first_word_offset_per_line[i] = index of first
    // auth word belonging to auth_text[i] in the flat auth_words array.
    // O(L) once instead of O(N*L*words) per anchor pair.
    let mut first_word_offset_per_line: Vec<usize> = Vec::with_capacity(auth_text.len());
    let mut running = 0usize;
    for line in &auth_text {
        first_word_offset_per_line.push(running);
        running += line.split_whitespace().count();
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

        // Use precomputed prefix-sum — O(1) lookup, invariant: always in-bounds.
        let local_idx = auth_idx - first_word_offset_per_line[auth_line_idx];
        // Safety: local_idx is derived from auth_word_to_line which was built
        // by iterating split_whitespace(), so the nth() is always in-bounds.
        let auth_word = auth_text[auth_line_idx]
            .split_whitespace()
            .nth(local_idx)
            .expect("auth_word_to_line invariant: auth_idx always in-bounds")
            .to_string();

        if Some(auth_line_idx) != current_auth_line {
            // Flush previous line
            if let Some(prev_li) = current_auth_line {
                let line_text = auth_text[prev_li].clone();
                let token_count = line_text.split_whitespace().count();
                // Per feedback_line_timing_only.md: emit words: None when the
                // collected word array doesn't cover every token in the auth
                // line text. Partial coverage misleads the karaoke renderer
                // (line has 4 tokens but words array has 3 anchored entries).
                // The renderer falls back to line-level highlighting.
                let words_out = if !current_words.is_empty() && current_words.len() == token_count {
                    Some(std::mem::take(&mut current_words))
                } else {
                    current_words.clear();
                    None
                };
                new_lines.push(AlignedLine {
                    text: line_text,
                    start_ms: current_start_ms,
                    end_ms: current_end_ms,
                    words: words_out,
                });
            }
            current_auth_line = Some(auth_line_idx);
            current_start_ms = start_ms;
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
        let line_text = auth_text[li].clone();
        let token_count = line_text.split_whitespace().count();
        let words_out = if !current_words.is_empty() && current_words.len() == token_count {
            Some(current_words)
        } else {
            None
        };
        new_lines.push(AlignedLine {
            text: line_text,
            start_ms: current_start_ms,
            end_ms: current_end_ms,
            words: words_out,
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
    // word_idx is derived from asr_word_origin which was built by iterating
    // the same words slice — so indexing is always in-bounds.
    let w = words
        .get(word_idx)
        .expect("asr_word_origin invariant: word_idx always in-bounds");
    (w.start_ms, w.end_ms, w.confidence)
}

/// Stable source-preference for tie-breaking in `best_authoritative`.
/// Higher score = more reliable timing / text quality.
fn source_priority(source: &str) -> u32 {
    if source.starts_with("tier1:spotify") {
        4
    } else if source.starts_with("tier1:lrclib") {
        3
    } else if source == "genius" {
        2
    } else if source.starts_with("tier1:yt_subs") {
        1
    } else {
        0
    }
}

/// Pick the strongest authoritative source: most lines wins; ties broken by
/// `source_priority` (spotify > lrclib > genius > yt_subs > other).
fn best_authoritative(candidates: &[CandidateText]) -> Vec<String> {
    candidates
        .iter()
        .max_by_key(|c| (c.lines.len(), source_priority(&c.source)))
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
        // "I've" is NOT in the words array (only 3 anchored entries for a
        // 4-token auth line) → per F2 / feedback_line_timing_only.md the
        // reconciler must emit words: None so the renderer falls back to
        // line-level highlighting rather than showing misleading partial data.
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
        // "I've" was not anchored → words array covers only 3 of 4 tokens →
        // incomplete coverage must produce words: None (not partial highlights).
        assert!(
            reconciled.lines[0].words.is_none(),
            "incomplete word coverage → line-level fallback per feedback_line_timing_only.md"
        );
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

    #[test]
    fn reconciler_handles_multi_line_authoritative() {
        // Two ASR lines + two auth lines. Exercises:
        //  - line-transition flush logic (the if-Some(prev_li) branch fires once)
        //  - first_word_offset_per_line for auth line_idx > 0 (second auth line
        //    starts at flat offset 3, not 0 — the old O(N*L) helper had to
        //    correctly sum across prior lines; the precomputed prefix-sum must
        //    produce the same result)
        //  - auth_idx - offset arithmetic: for auth_idx=3 (first word of line 2)
        //    local_idx must be 0, not 3.
        //  - Both lines flush separately with correct start_ms windows.
        //
        // Auth line 0: "amazing grace" (2 tokens, both anchored → words: Some)
        // Auth line 1: "how sweet the sound" (4 tokens, all anchored → words: Some)
        //
        // ASR line 0: "amazing grace" — matches exactly
        // ASR line 1: "how sweet the sound" — matches exactly
        let asr = make_asr(&[
            (
                "amazing grace",
                0,
                2000,
                &[("amazing", 0, 1000), ("grace", 1000, 2000)],
            ),
            (
                "how sweet the sound",
                2000,
                6000,
                &[
                    ("how", 2000, 3000),
                    ("sweet", 3000, 4000),
                    ("the", 4000, 5000),
                    ("sound", 5000, 6000),
                ],
            ),
        ]);
        let auth = vec![CandidateText {
            source: "tier1:lrclib".into(),
            lines: vec!["amazing grace".into(), "how sweet the sound".into()],
            line_timings: None,
            has_timing: false,
        }];
        let reconciled = reconcile(&asr, &auth);

        // Both auth lines must be present
        assert_eq!(reconciled.lines.len(), 2, "expected 2 reconciled lines");

        // Line 0: "amazing grace" — all 2 tokens anchored → words: Some
        assert_eq!(reconciled.lines[0].text, "amazing grace");
        assert_eq!(reconciled.lines[0].start_ms, 0);
        assert_eq!(reconciled.lines[0].end_ms, 2000);
        assert!(
            reconciled.lines[0].words.is_some(),
            "line 0: full token coverage → words should be Some"
        );
        let w0 = reconciled.lines[0].words.as_ref().unwrap();
        assert_eq!(w0.len(), 2);
        assert_eq!(w0[0].text, "amazing");
        assert_eq!(w0[1].text, "grace");

        // Line 1: "how sweet the sound" — auth flat offset starts at 2
        // (first_word_offset_per_line[1] = 2), so local_idx for auth_idx=2
        // must be 0 ("how"), not 2. All 4 tokens anchored → words: Some.
        assert_eq!(reconciled.lines[1].text, "how sweet the sound");
        assert_eq!(
            reconciled.lines[1].start_ms, 2000,
            "line 1 must start at 'how' anchor timing"
        );
        assert_eq!(reconciled.lines[1].end_ms, 6000);
        assert!(
            reconciled.lines[1].words.is_some(),
            "line 1: full token coverage → words should be Some"
        );
        let w1 = reconciled.lines[1].words.as_ref().unwrap();
        assert_eq!(w1.len(), 4);
        assert_eq!(w1[0].text, "how");
        assert_eq!(w1[3].text, "sound");

        assert!(reconciled.provenance.ends_with("+reconciled"));
    }
}
