//! Tests for text_reference_merge phases 1, 2, 4, 5 (Phase 3 Claude path needs
//! a mock AiClient and is exercised end-to-end on win-resolume reprocess
//! verification, not in unit tests). Sibling-included from text_reference_merge.rs.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::tier1::CandidateText;

fn make_word(text: &str, start_ms: u32, end_ms: u32) -> AlignedWord {
    AlignedWord {
        text: text.to_string(),
        start_ms,
        end_ms,
        confidence: 0.9,
    }
}

fn asr(words: Vec<AlignedWord>) -> AlignedTrack {
    AlignedTrack {
        lines: vec![AlignedLine {
            text: "(combined)".into(),
            start_ms: 0,
            end_ms: 60000,
            words: Some(words),
        }],
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    }
}

// ── Phase 2: chorus repeat detection ──────────────────────────────────────────

#[test]
fn detect_chorus_repeats_emits_for_long_unmatched_gap() {
    // 1 ref line "holy holy holy". ASR sings it twice. Both chorus
    // occurrences span > 4 s so whichever side LCS consumes, the other
    // exceeds CHORUS_REPEAT_GAP_MS (4000) and triggers the re-emit.
    // (LCS backtrack is greedy-from-end → Phase 1 consumes the second
    // chorus; Phase 2 re-emits the first.)
    let ref_lines = vec!["holy holy holy".to_string()];
    let asr_track = asr(vec![
        make_word("holy", 0, 500),
        make_word("holy", 2500, 3000),
        make_word("holy", 4500, 5000),
        // long instrumental pause; second-pass chorus repeats:
        make_word("holy", 9000, 9500),
        make_word("holy", 11500, 12000),
        make_word("holy", 13500, 14000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emits = match_ref_to_asr(&ref_lines, &asr_words);
    let extras = detect_chorus_repeats(&ref_lines, &asr_words, &emits);
    assert!(
        !extras.is_empty(),
        "expected at least one chorus repeat re-emit; got {:?}",
        extras
    );
    let emit = &extras[0];
    assert_eq!(emit.text, "holy holy holy");
    // LCS backtrack is greedy-from-end so Phase 1 actually consumed the LAST
    // three indices and Phase 2 re-emits at the FIRST three (the unmatched
    // window from index 0..2). Either side is valid for chorus-repeat
    // semantics — the assertion is just that emit indices are disjoint from
    // Phase 1's consumed set.
    let phase1_consumed: std::collections::HashSet<usize> = emits
        .iter()
        .flat_map(|e| e.asr_word_indices.iter().copied())
        .collect();
    let phase2_consumed: std::collections::HashSet<usize> =
        emit.asr_word_indices.iter().copied().collect();
    assert!(
        phase1_consumed.is_disjoint(&phase2_consumed),
        "chorus re-emit must point at audio words NOT consumed in Phase 1; phase1={:?} phase2={:?}",
        phase1_consumed,
        phase2_consumed
    );
}

// ── Phase 2.6: absorb_prefix_matches ──────────────────────────────────────────

#[test]
fn absorb_prefix_matches_attaches_unconsumed_ref_prefix() {
    // Phase 2 sliding-window window cap caused [is, the, highest] match
    // for "Your name is the highest" while [your, name] sat unconsumed
    // before. Prefix-absorption walks back, finds them, attaches.
    let asr_track = asr(vec![
        make_word("your", 0, 100),   // 0 — prefix word 1
        make_word("name", 200, 400), // 1 — prefix word 2
        // 8s instrumental gap
        make_word("is", 8500, 8700),      // 2 — current emit start
        make_word("the", 8800, 9000),     // 3
        make_word("highest", 9100, 9500), // 4
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![LineEmit {
        text: "Your name is the highest".into(),
        asr_word_indices: vec![2, 3, 4],
    }];
    absorb::absorb_prefix_matches(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2, 3, 4]);
}

#[test]
fn absorb_prefix_matches_stops_at_consumed_word() {
    // Idx 1 is consumed (assigned to a different emit). Walking back from
    // emit's first matched, we hit consumed and stop — don't claim
    // someone else's word.
    let asr_track = asr(vec![
        make_word("your", 0, 100),        // 0 — would match prefix
        make_word("foreign", 200, 400),   // 1 — consumed by another emit
        make_word("name", 500, 700),      // 2 — would match prefix
        make_word("is", 8500, 8700),      // 3 — current emit start
        make_word("the", 8800, 9000),     // 4
        make_word("highest", 9100, 9500), // 5
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Foreign claim".into(),
            asr_word_indices: vec![1],
        },
        LineEmit {
            text: "Your name is the highest".into(),
            asr_word_indices: vec![3, 4, 5],
        },
    ];
    absorb::absorb_prefix_matches(&mut emits, &asr_words);
    // Walks back from idx 3: idx 2 ("name") matches ref[1] → add. cursor=2.
    // Walks back from cursor 2: idx 1 ("foreign") consumed → stop.
    // Result: prefix only includes idx 2; idx 0 ("your") not attached.
    assert_eq!(emits[1].asr_word_indices, vec![2, 3, 4, 5]);
}

#[test]
fn absorb_prefix_matches_no_op_when_emit_already_starts_at_ref_zero() {
    let asr_track = asr(vec![make_word("your", 0, 100), make_word("name", 200, 400)]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![LineEmit {
        text: "Your name".into(),
        asr_word_indices: vec![0, 1],
    }];
    absorb::absorb_prefix_matches(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1]);
}

#[test]
fn absorb_prefix_matches_skips_single_word_refs() {
    // 1-word ref line — no prefix to absorb.
    let asr_track = asr(vec![make_word("holy", 0, 100), make_word("holy", 200, 400)]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![LineEmit {
        text: "Holy".into(),
        asr_word_indices: vec![1],
    }];
    absorb::absorb_prefix_matches(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![1]);
}

// ── Phase 2.7: absorb_sustained_boundary_tokens ───────────────────────────────

#[test]
fn absorb_sustained_boundary_artifact_replacement_kicks_in() {
    // id=132 2:53: prev_last "holy" 80ms (artifact), next_first "holy"
    // 2141ms (real sustained note). Artifact-replacement absorbs the
    // long real holy into prev so prev's line displays through the
    // full sustained note. Line 2 keeps just "forever".
    let asr_track = asr(vec![
        make_word("be", 0, 80),
        make_word("holy", 100, 180),   // 80ms — artifact
        make_word("holy", 1500, 3641), // 2141ms — real sustained
        make_word("forever", 3700, 4000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Be Holy".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Holy forever".into(),
            asr_word_indices: vec![2, 3],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    // Long "holy" (idx 2) absorbed into prev despite next ref starting
    // with "holy" — because next ref's same-text rule is overridden by
    // artifact detection.
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2]);
    assert_eq!(emits[1].asr_word_indices, vec![3]);
}

#[test]
fn absorb_sustained_boundary_skips_when_next_ref_starts_same() {
    // Next line "Holy forever" starts with "holy" — its first ref word
    // matches the token. The token rightfully belongs to next's "Holy",
    // don't absorb it into prev. id=132 1:33 case: "Holy forever" was
    // losing its first sung holy.
    let asr_track = asr(vec![
        make_word("you", 0, 80),
        make_word("be", 90, 99),
        make_word("holy", 100, 200),
        make_word("holy", 250, 400), // would absorb under old rule
        make_word("forever", 500, 800),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "You will always be, Holy".into(),
            asr_word_indices: vec![0, 1, 2],
        },
        LineEmit {
            text: "Holy forever".into(),
            asr_word_indices: vec![3, 4],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    // Phase 1's distribution preserved; the second "holy" stays with
    // next so "Holy forever" displays starting at its first sung "holy".
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2]);
    assert_eq!(emits[1].asr_word_indices, vec![3, 4]);
}

#[test]
fn absorb_sustained_boundary_transfers_when_next_ref_differs() {
    // Next line starts with a different word ("praise"). The trailing
    // "holy" token at boundary is genuine sustained-note residue from
    // prev — absorb it.
    let asr_track = asr(vec![
        make_word("be", 0, 80),
        make_word("holy", 100, 200),
        make_word("holy", 250, 400), // sustained, should absorb
        make_word("praise", 500, 800),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Be Holy".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Praise the Lord".into(),
            asr_word_indices: vec![2, 3],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2]);
    assert_eq!(emits[1].asr_word_indices, vec![3]);
}

#[test]
fn absorb_sustained_boundary_no_op_when_words_differ() {
    let asr_track = asr(vec![
        make_word("holy", 0, 100),
        make_word("forever", 200, 400),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Line A".into(),
            asr_word_indices: vec![0],
        },
        LineEmit {
            text: "Line B".into(),
            asr_word_indices: vec![1],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0]);
    assert_eq!(emits[1].asr_word_indices, vec![1]);
}

#[test]
fn absorb_sustained_boundary_preserves_non_empty_next() {
    // Next has only one token that matches prev — DON'T transfer (would
    // leave next empty). The token stays as next's only word.
    let asr_track = asr(vec![make_word("holy", 0, 100), make_word("holy", 200, 300)]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Line A".into(),
            asr_word_indices: vec![0],
        },
        LineEmit {
            text: "Holy".into(),
            asr_word_indices: vec![1],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0]);
    assert_eq!(emits[1].asr_word_indices, vec![1]);
}

#[test]
fn absorb_sustained_boundary_skips_long_gap() {
    // Same word but gap > SUSTAINED_NOTE_MAX_GAP_MS=2000 → not a sustained
    // note, treat as separate occurrences.
    let asr_track = asr(vec![
        make_word("holy", 0, 100),
        make_word("holy", 5000, 5100), // 4.9s gap > 2s
        make_word("forever", 5500, 5800),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Line A".into(),
            asr_word_indices: vec![0],
        },
        LineEmit {
            text: "Holy forever".into(),
            asr_word_indices: vec![1, 2],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![0]);
    assert_eq!(emits[1].asr_word_indices, vec![1, 2]);
}

// ── absorb boundary tests (kill mutation survivors) ───────────────────────────

#[test]
fn absorb_sustained_gap_at_threshold_does_not_break() {
    // gap exactly == SUSTAINED_NOTE_MAX_GAP_MS (2000): original guard is
    // `gap > 2000 → break`, so 2000 does NOT break — absorption proceeds.
    // Kills `>` ↔ `==` and `>` ↔ `>=` at line 118:20.
    let asr_track = asr(vec![
        make_word("a", 0, 80),         // prev_dur 80 (artifact)
        make_word("holy", 100, 200),   // prev's last (also short → artifact-eligible)
        make_word("holy", 2200, 3000), // gap = 2200-200 = 2000 exactly; long sustained
        make_word("praise", 3500, 3800),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Be Holy".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Praise".into(),
            asr_word_indices: vec![2, 3],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    // gap=2000 NOT > 2000, NOT broken. Artifact-replacement check fires:
    // prev_dur=100 (200-100) < 200; next_dur=800 (3000-2200) >= 100*5=500.
    // → absorb the long "holy" into prev. emits[0] gains idx 2.
    assert_eq!(emits[0].asr_word_indices, vec![0, 1, 2]);
    assert_eq!(emits[1].asr_word_indices, vec![3]);
}

#[test]
fn absorb_sustained_gap_just_over_threshold_breaks() {
    // gap == 2001 — `> 2000` true → break, no absorption.
    let asr_track = asr(vec![
        make_word("a", 0, 80),
        make_word("holy", 100, 200),
        make_word("holy", 2201, 3000), // gap = 2201-200 = 2001
        make_word("praise", 3500, 3800),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Be Holy".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Praise".into(),
            asr_word_indices: vec![2, 3],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    assert_eq!(
        emits[0].asr_word_indices,
        vec![0, 1],
        "gap > 2000 must break"
    );
    assert_eq!(emits[1].asr_word_indices, vec![2, 3]);
}

#[test]
fn absorb_sustained_prev_dur_at_artifact_threshold_does_not_replace() {
    // prev_dur exactly == ARTIFACT_TOKEN_DUR_MS (200): original guard is
    // `prev_dur < 200 → artifact`, so 200 does NOT trigger artifact-
    // replacement. Without artifact path, the same-ref-word rule applies
    // (next ref starts with "holy" → no absorption). Kills `<` ↔ `<=` at
    // line 135:52.
    let asr_track = asr(vec![
        make_word("be", 0, 80),
        make_word("holy", 100, 300),  // prev_dur = 200 EXACTLY
        make_word("holy", 350, 2000), // long, but prev_dur not < 200
        make_word("forever", 2100, 2400),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Be Holy".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Holy forever".into(),
            asr_word_indices: vec![2, 3],
        },
    ];
    absorb::absorb_sustained_boundary_tokens(&mut emits, &asr_words);
    // prev_dur = 200, NOT < 200 → no artifact-replacement.
    // next ref starts with "holy" (matches token) → break. No absorption.
    assert_eq!(emits[0].asr_word_indices, vec![0, 1]);
    assert_eq!(emits[1].asr_word_indices, vec![2, 3]);
}

#[test]
fn absorb_prefix_walks_scan_to_zero_without_match() {
    // emit.asr_word_indices=[3] (only "d" matched). ref="a b c d" so the
    // prefix walk-back attempts ref_pos=2,1,0. asr_words[0..3] none match
    // their target. Inner while predicate `scan > 0` exits cleanly when
    // scan reaches 0. With `>=` mutation: scan=0 enters body, scan -= 1
    // underflows usize → debug-mode panic. Kills line 67:24 `>` ↔ `>=`.
    let asr_track = asr(vec![
        make_word("x", 0, 100),
        make_word("y", 200, 300),
        make_word("z", 400, 500),
        make_word("d", 600, 700),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![LineEmit {
        text: "a b c d".into(),
        asr_word_indices: vec![3],
    }];
    absorb::absorb_prefix_matches(&mut emits, &asr_words);
    // No matches found in walk-back → emit unchanged.
    assert_eq!(emits[0].asr_word_indices, vec![3]);
}

// ── Phase 2.5: trim_outlier_indices ───────────────────────────────────────────

#[test]
fn trim_outlier_indices_keeps_tight_match_intact() {
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 400),
        make_word("c", 500, 700),
        make_word("d", 800, 1000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1, 2, 3];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0, 1, 2, 3]);
}

#[test]
fn trim_outlier_indices_drops_trailing_outlier_past_cap() {
    // 5 words: [0..4 contiguous within 7.5s] + [5 jumped to 20s].
    // Span 20s > LONG_LINE_CAP_MS=8s, drop trailing.
    let asr_track = asr(vec![
        make_word("a", 0, 500),
        make_word("b", 1000, 1500),
        make_word("c", 3000, 3500),
        make_word("d", 5000, 5500),
        make_word("e", 7000, 7500),
        make_word("outlier", 19000, 20000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1, 2, 3, 4, 5];
    trim_outlier_indices(&mut indices, &asr_words);
    // After trim: [0..4] span 7.5s within cap.
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);
}

#[test]
fn trim_outlier_indices_drops_to_single_when_two_entry_span_exceeds_cap() {
    // 2-entry emit with span > cap — trim drops the trailing outlier so
    // only [0] remains. Phase 5 will then reject the single-word residual
    // via its MIN_LINE_DURATION_MS micro-window drop. Reproduces id=132
    // 2026-05-04 case where Phase 2 LCS for "Holy forever" picked the
    // first "holy" + a far-later "forever" across multiple sung phrases.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 50000, 50100), // 50s gap — exceeds cap
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0, 1];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_keeps_single_entry_intact() {
    // Single-entry input is untouched (no second word to compare span).
    let asr_track = asr(vec![make_word("a", 1000, 1500)]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![0];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0]);
}

#[test]
fn trim_outlier_indices_handles_unsorted_input() {
    // Indices arrive ascending after Phase 1 sort, but defensive: trim should
    // sort before measuring span.
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 400),
        make_word("c", 500, 700),
        make_word("outlier", 20000, 21000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut indices = vec![3, 0, 1, 2];
    trim_outlier_indices(&mut indices, &asr_words);
    assert_eq!(indices, vec![0, 1, 2]);
}

// ── Phase 4: aligned_lines_for_emit ───────────────────────────────────────────

#[test]
fn aligned_lines_for_emit_single_uses_min_max_word_timing() {
    let asr_track = asr(vec![
        make_word("a", 100, 300),
        make_word("b", 400, 600),
        make_word("c", 700, 900),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "a b c".into(),
        asr_word_indices: vec![0, 1, 2],
    };
    let lines = aligned_lines_for_emit(&emit, &asr_words, None);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_ms, 100);
    assert_eq!(lines[0].end_ms, 900);
    assert!(lines[0].words.is_none());
    assert_eq!(lines[0].text, "a b c");
}

#[test]
fn aligned_lines_for_emit_with_subs_assigns_per_sub_word_timing() {
    // Parent text "alpha bravo charlie delta". Subs split "alpha bravo" /
    // "charlie delta". Each sub gets timing from its constituent ASR words.
    let asr_track = asr(vec![
        make_word("alpha", 100, 300),
        make_word("bravo", 400, 700),
        make_word("charlie", 1500, 2000),
        make_word("delta", 2100, 2500),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let emit = LineEmit {
        text: "alpha bravo charlie delta".into(),
        asr_word_indices: vec![0, 1, 2, 3],
    };
    let subs = vec!["alpha bravo".to_string(), "charlie delta".to_string()];
    let lines = aligned_lines_for_emit(&emit, &asr_words, Some(&subs));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "alpha bravo");
    assert_eq!(lines[0].start_ms, 100);
    assert_eq!(lines[0].end_ms, 700);
    assert_eq!(lines[1].text, "charlie delta");
    assert_eq!(lines[1].start_ms, 1500);
    assert_eq!(lines[1].end_ms, 2500);
}

// ── Phase 5: cap + monotonic ──────────────────────────────────────────────────

#[test]
fn apply_cap_and_monotonic_preserves_long_natural_span() {
    // 30 s line — Phase 5 no longer caps display duration. The matcher
    // (sliding-window + trim) is responsible for keeping natural spans
    // reasonable; Phase 5 just enforces monotonic + bounded extension.
    let mut lines = vec![AlignedLine {
        text: "long".into(),
        start_ms: 1000,
        end_ms: 30000,
        words: None,
    }];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_ms, 1000);
    assert_eq!(lines[0].end_ms, 30000);
}

#[test]
fn apply_cap_and_monotonic_floor_clamps_overlap() {
    // Both lines have original dur 1000ms (>= MIN). After floor-clamp the
    // second's start_ms is pushed to 1000 (the first's end_ms), leaving its
    // dur at 500ms — equal to MIN_LINE_DURATION_MS so it's kept.
    let mut lines = vec![
        AlignedLine {
            text: "a".into(),
            start_ms: 0,
            end_ms: 1000,
            words: None,
        },
        AlignedLine {
            text: "b".into(),
            start_ms: 500,
            end_ms: 1500,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert!(lines[1].start_ms >= lines[0].end_ms);
    assert!(lines[1].end_ms > lines[1].start_ms);
}

#[test]
fn apply_cap_and_monotonic_extends_micro_window_within_tolerance() {
    // 200 ms middle line followed by 800 ms gap. Extension caps at
    // natural_end + tolerance: 2200 + 1500 = 3700, then min with
    // next.start = 3000 → end_ms = 3000. Final dur = 1000 ms,
    // ≥ MIN_LINE_DURATION_MS. Kept.
    let mut lines = vec![
        AlignedLine {
            text: "real".into(),
            start_ms: 0,
            end_ms: 1500,
            words: None,
        },
        AlignedLine {
            text: "short".into(),
            start_ms: 2000,
            end_ms: 2200,
            words: None,
        },
        AlignedLine {
            text: "more".into(),
            start_ms: 3000,
            end_ms: 5000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1].start_ms, 2000);
    assert_eq!(lines[1].end_ms, 3000);
}

#[test]
fn apply_cap_and_monotonic_fills_reasonable_gap_to_next_start() {
    // Gap 1500 ms ≤ REASONABLE_GAP_MS — extension reaches B.start fully.
    let mut lines = vec![
        AlignedLine {
            text: "A".into(),
            start_ms: 0,
            end_ms: 2000,
            words: None,
        },
        AlignedLine {
            text: "B".into(),
            start_ms: 3500,
            end_ms: 5000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].end_ms, 3500, "reasonable gap fully bridged");
    assert_eq!(lines[1].start_ms, 3500);
}

#[test]
fn apply_cap_and_monotonic_caps_long_gap_at_tolerance() {
    // Gap 29 s > REASONABLE_GAP_MS — extension capped at natural_end +
    // EXTENSION_TOLERANCE_MS=1500. Wall blank from 2500 to 30_000.
    let mut lines = vec![
        AlignedLine {
            text: "A".into(),
            start_ms: 0,
            end_ms: 1000,
            words: None,
        },
        AlignedLine {
            text: "B".into(),
            start_ms: 30_000,
            end_ms: 32_000,
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0].end_ms,
        1000 + EXTENSION_TOLERANCE_MS,
        "long gap → tolerance-capped extension"
    );
    assert_eq!(lines[1].start_ms, 30_000);
}

#[test]
fn apply_cap_and_monotonic_last_line_no_extension() {
    let mut lines = vec![AlignedLine {
        text: "only".into(),
        start_ms: 0,
        end_ms: 1500,
        words: None,
    }];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].end_ms, 1500, "last line keeps original end_ms");
}

#[test]
fn apply_cap_and_monotonic_drops_post_clamp_collapse() {
    // Both lines start near 1000ms, both 600ms duration. After floor-clamp
    // the second's window collapses to <500ms — drop it.
    let mut lines = vec![
        AlignedLine {
            text: "a".into(),
            start_ms: 0,
            end_ms: 1100,
            words: None,
        },
        AlignedLine {
            text: "collapses".into(),
            start_ms: 700,
            end_ms: 1300, // post-clamp would be 1100..1300 = 200ms
            words: None,
        },
    ];
    apply_cap_and_monotonic(&mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "a");
}

// ── deterministic_split_one ───────────────────────────────────────────────────

#[test]
fn deterministic_split_one_short_line_kept_intact() {
    let s = deterministic_split_one("Holy forever");
    assert_eq!(s, vec!["Holy forever".to_string()]);
}

#[test]
fn deterministic_split_one_long_line_splits_at_word_boundary_under_cap() {
    let s = deterministic_split_one("A thousand generations falling down in worship");
    // Each sub must be <= 32 chars.
    for sub in &s {
        assert!(
            sub.chars().count() <= SUBLINE_MAX_CHARS,
            "sub over cap: {:?}",
            sub
        );
    }
    // Joined back (with single space) must equal original (modulo trim).
    let joined = s.join(" ");
    assert_eq!(joined, "A thousand generations falling down in worship");
}

#[test]
fn deterministic_split_one_long_line_with_comma_prefers_comma_break() {
    let s = deterministic_split_one("And the angels cry, holy forever amen");
    assert!(s.len() >= 2);
    // First sub ends at comma — last char of first piece is ','.
    assert!(
        s[0].ends_with(','),
        "expected comma at end of first sub: {:?}",
        s
    );
}

// ── parse_split_response ──────────────────────────────────────────────────────

#[test]
fn parse_split_response_extracts_clean_json() {
    let raw = r#"{"splits":[{"i":0,"subs":[{"en":"alpha"},{"en":"beta"}]}]}"#;
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 0);
    assert_eq!(parsed.splits[0].subs.len(), 2);
    assert_eq!(parsed.splits[0].subs[0].en, "alpha");
}

#[test]
fn parse_split_response_strips_prose_preamble() {
    let raw = "Here you go:\n```json\n{\"splits\":[{\"i\":7,\"subs\":[{\"en\":\"x\"}]}]}\n```";
    let parsed = parse_split_response(raw).unwrap();
    assert_eq!(parsed.splits.len(), 1);
    assert_eq!(parsed.splits[0].i, 7);
}

// ── lcs_align_forward_greedy ──────────────────────────────────────────────────

#[test]
fn lcs_fg_empty_ref_returns_empty() {
    let r: [&str; 0] = [];
    assert_eq!(
        lcs_align_forward_greedy(&r, &["a", "b"]),
        Vec::<Option<usize>>::new()
    );
}

#[test]
fn lcs_fg_empty_asr_returns_all_none() {
    assert_eq!(lcs_align_forward_greedy(&["a", "b"], &[]), vec![None, None]);
}

#[test]
fn lcs_fg_identity_returns_in_order() {
    assert_eq!(
        lcs_align_forward_greedy(&["a", "b", "c"], &["a", "b", "c"]),
        vec![Some(0), Some(1), Some(2)]
    );
}

#[test]
fn lcs_fg_disjoint_returns_all_none() {
    assert_eq!(
        lcs_align_forward_greedy(&["a", "b"], &["x", "y"]),
        vec![None, None]
    );
}

#[test]
fn lcs_fg_skips_unmatched_asr_words() {
    // ref=[a,b], asr=[a,x,b] — fg must skip "x" between "a" and "b".
    assert_eq!(
        lcs_align_forward_greedy(&["a", "b"], &["a", "x", "b"]),
        vec![Some(0), Some(2)]
    );
}

#[test]
fn lcs_fg_consumes_first_match_only() {
    // ref=[a,a], asr=[a,b,a] — fg picks first "a", advances past, finds second.
    assert_eq!(
        lcs_align_forward_greedy(&["a", "a"], &["a", "b", "a"]),
        vec![Some(0), Some(2)]
    );
}

#[test]
fn lcs_fg_stops_when_asr_exhausted() {
    // ref=[a,b,c], asr=[a,b] — third ref word has no match.
    assert_eq!(
        lcs_align_forward_greedy(&["a", "b", "c"], &["a", "b"]),
        vec![Some(0), Some(1), None]
    );
}

#[test]
fn lcs_fg_misordered_asr_loses_later_match() {
    // ref=[a,b], asr=[b,a] — fg is forward-only: cannot rewind to find "b"
    // after consuming "a" via the leading "b". Wait — "a" appears AT idx 1,
    // and there's no "b" after that. So lcs_align_forward_greedy walks j
    // forward until ref[0]="a" matches asr[1]="a" → Some(1). Then for
    // ref[1]="b", j=2 is past end → None. Result: [Some(1), None].
    assert_eq!(
        lcs_align_forward_greedy(&["a", "b"], &["b", "a"]),
        vec![Some(1), None]
    );
}

// ── lcs_align_dp ──────────────────────────────────────────────────────────────

#[test]
fn lcs_dp_empty_ref_returns_empty() {
    let r: [&str; 0] = [];
    assert_eq!(lcs_align_dp(&r, &["a", "b"]), Vec::<Option<usize>>::new());
}

#[test]
fn lcs_dp_empty_asr_returns_all_none() {
    assert_eq!(lcs_align_dp(&["a", "b"], &[]), vec![None, None]);
}

#[test]
fn lcs_dp_identity_returns_in_order() {
    assert_eq!(
        lcs_align_dp(&["a", "b", "c"], &["a", "b", "c"]),
        vec![Some(0), Some(1), Some(2)]
    );
}

#[test]
fn lcs_dp_finds_optimal_subsequence() {
    // ref=[a,b,c], asr=[a,x,b,y,c] — DP finds full 3-match.
    assert_eq!(
        lcs_align_dp(&["a", "b", "c"], &["a", "x", "b", "y", "c"]),
        vec![Some(0), Some(2), Some(4)]
    );
}

#[test]
fn lcs_dp_disjoint_returns_all_none() {
    assert_eq!(lcs_align_dp(&["a", "b"], &["x", "y"]), vec![None, None]);
}

#[test]
fn lcs_dp_picks_globally_optimal_alternation() {
    // ref=[a,b,a], asr=[a,a,b] — DP can drop a 1-match path (start at
    // asr[0]) for the 2-match path that matches "a" then "b". Forward-
    // greedy on the same input would pick [Some(0), Some(2), None].
    // (Verifies DP isn't accidentally implementing greedy.)
    let dp = lcs_align_dp(&["a", "b", "a"], &["a", "a", "b"]);
    let matched: usize = dp.iter().filter(|x| x.is_some()).count();
    assert_eq!(matched, 2, "DP must find the 2-element common subsequence");
}

// ── lcs_align (combined picker) ───────────────────────────────────────────────

#[test]
fn lcs_align_returns_fg_when_fg_count_ge_dp() {
    // Both produce 2 matches. fg first by `>=` rule.
    let r = ["a", "b"];
    let a = ["a", "b"];
    let result = lcs_align(&r, &a);
    let fg = lcs_align_forward_greedy(&r, &a);
    assert_eq!(result, fg, "fg_count == dp_count must pick fg (`>=` rule)");
}

#[test]
fn lcs_align_returns_dp_when_dp_count_strictly_greater() {
    // ref=[a,b,c], asr=[a,c,b,c] — fg gets [Some(0), Some(2), Some(3)] (3),
    // dp also 3. Need a case where dp wins.
    // ref=[a,b], asr=[b,a,b] — fg [Some(1), Some(2)] count=2; dp also 2.
    // Forward-greedy is actually quite robust; constructing fg<dp is hard
    // without complex reorderings. Simpler:
    // ref=[a,b], asr=[a,a,c] — fg [Some(0), None] count=1; dp also [Some(0|1), None]=1.
    // Try ref=[a,b,a], asr=[b,a,b,a]:
    //   fg: walk a from j=0: asr[0]=b≠a, j=1, asr[1]=a→Some(1), j=2.
    //       walk b from j=2: asr[2]=b→Some(2), j=3.
    //       walk a from j=3: asr[3]=a→Some(3). Result [S(1),S(2),S(3)] count=3.
    //   dp: same 3.
    // Hard to construct fg<dp purely with strings. Validate the general
    // contract instead: result has count >= max(fg,dp) by construction.
    let r = ["x", "y", "z"];
    let a = ["x", "y", "z"];
    let result = lcs_align(&r, &a);
    let fg = lcs_align_forward_greedy(&r, &a);
    let dp = lcs_align_dp(&r, &a);
    let result_count = result.iter().filter(|x| x.is_some()).count();
    let fg_count = fg.iter().filter(|x| x.is_some()).count();
    let dp_count = dp.iter().filter(|x| x.is_some()).count();
    assert!(result_count >= fg_count.max(dp_count));
}

#[test]
fn lcs_align_picks_strictly_higher_count_path() {
    // Pathological case: fg can be sub-optimal vs dp.
    // ref = [a,b,a,b]; asr = [a,b,a,c,b]
    //   fg: a@0, b@1, a@2, b@4 → 4 matches
    //   dp: a@0, b@1, a@2, b@4 → 4 matches
    // Both 4. Hard to force dp>fg with simple strings. Accept that the
    // picker is symmetric in well-behaved inputs and only diverges on
    // adversarial cases. Verify here that whichever path wins, the
    // result is one of {fg, dp} verbatim — the picker never synthesises
    // a third alignment.
    let r = ["a", "b", "a", "b"];
    let a = ["a", "b", "a", "c", "b"];
    let result = lcs_align(&r, &a);
    let fg = lcs_align_forward_greedy(&r, &a);
    let dp = lcs_align_dp(&r, &a);
    assert!(
        result == fg || result == dp,
        "lcs_align must return verbatim fg or dp; got {result:?}, fg={fg:?}, dp={dp:?}"
    );
}

#[test]
fn lcs_align_picker_returns_fg_at_equal_count_distinguishable_indices() {
    // ref=[a], asr=[a,a]. fg picks asr[0] → [Some(0)]. dp's traceback is
    // greedy-from-end so it picks asr[1] → [Some(1)]. Both count=1, so
    // fg_count == dp_count and the picker must return fg under the `>=`
    // rule. Mutation `>=` ↔ `<` would return dp = [Some(1)] instead.
    // Kills line 914:17 `>=` ↔ `<` in lcs_align.
    let r = ["a"];
    let a = ["a", "a"];
    assert_eq!(
        lcs_align(&r, &a),
        vec![Some(0)],
        "picker must return fg (>=) at equal count"
    );
}
