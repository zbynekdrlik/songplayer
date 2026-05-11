//! Tests for Phase 2.65 `absorb::absorb_leading_unmatched`.
//! Sibling-included from text_reference_merge.rs to keep
//! text_reference_merge_tests.rs under the 1000-line file-size cap.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};

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

#[test]
fn absorb_leading_unmatched_claims_misheard_lead_words() {
    // id=21 2:12 regression: whisperx mistranscribed "shadow" as
    // "shed on" (asr[81-82]); LCS could not match either, so the line
    // started at "me" (asr[83]) instead of singer's first audible
    // sound at "shadow" (asr[81]). Phase 2.65 walks back through
    // unmatched ASR words and attaches them to the next emit so its
    // natural start moves back to the singer's true line-start.
    let asr_track = asr(vec![
        make_word("and", 128459, 128579),
        make_word("your", 128639, 129038),
        make_word("goodness", 129079, 130039),
        make_word("and", 130580, 130759),
        make_word("mercy", 130800, 131661),
        make_word("shed", 131741, 132541),
        make_word("on", 132781, 132941),
        make_word("me", 132961, 133401),
        make_word("for", 133942, 134222),
        make_word("all", 134502, 134742),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "And Your goodness and mercy".into(),
            asr_word_indices: vec![0, 1, 2, 3, 4],
        },
        LineEmit {
            text: "shadow me for all my history".into(),
            asr_word_indices: vec![7, 8, 9],
        },
    ];
    absorb::absorb_leading_unmatched(&mut emits, &asr_words);
    assert_eq!(
        emits[1].asr_word_indices,
        vec![5, 6, 7, 8, 9],
        "unmatched 'shed', 'on' must attach to 'shadow me…'"
    );
    assert_eq!(
        emits[0].asr_word_indices,
        vec![0, 1, 2, 3, 4],
        "prev emit unchanged"
    );
}

#[test]
fn absorb_leading_unmatched_stops_at_prev_consumed_boundary() {
    // Walk back must NOT cross prev's last matched word. Trigger
    // requires ref[0] not first-matched: ref[0]="line" not in asr,
    // first match is "next" = ref[1].
    let asr_track = asr(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 300),
        make_word("filler", 400, 500),
        make_word("next", 600, 700),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "A B".into(),
            asr_word_indices: vec![0, 1],
        },
        LineEmit {
            text: "Line next end".into(),
            asr_word_indices: vec![3],
        },
    ];
    absorb::absorb_leading_unmatched(&mut emits, &asr_words);
    assert_eq!(emits[1].asr_word_indices, vec![2, 3]);
    assert_eq!(emits[0].asr_word_indices, vec![0, 1]);
}

#[test]
fn absorb_leading_unmatched_caps_at_lookback_window() {
    // Walk back must respect LEADIN_MAX_MS (1.5 s). Trigger requires
    // ref[0] not first-matched: ref[0]="lost" not in asr, first match
    // is "found" = ref[1].
    let asr_track = asr(vec![
        make_word("very_old", 0, 100),
        make_word("close", 4500, 4900),
        make_word("found", 5000, 5300),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![LineEmit {
        text: "Lost found".into(),
        asr_word_indices: vec![2],
    }];
    absorb::absorb_leading_unmatched(&mut emits, &asr_words);
    assert_eq!(emits[0].asr_word_indices, vec![1, 2]);
}

#[test]
fn absorb_leading_unmatched_skips_when_ref0_already_matched() {
    // Skip branch: ref[0] equals first-matched ASR word so no leading
    // gap to fill. Vibrato/sustain tails of prev line mistranscribed
    // as random syllables must NOT attach to next.
    let asr_track = asr(vec![
        make_word("holy", 0, 800),
        make_word("ee", 900, 1100),
        make_word("forever", 1200, 2000),
    ]);
    let asr_words = flatten_asr(&asr_track);
    let mut emits = vec![
        LineEmit {
            text: "Holy".into(),
            asr_word_indices: vec![0],
        },
        LineEmit {
            text: "Forever".into(),
            asr_word_indices: vec![2],
        },
    ];
    absorb::absorb_leading_unmatched(&mut emits, &asr_words);
    assert_eq!(
        emits[1].asr_word_indices,
        vec![2],
        "vibrato tail must NOT attach"
    );
}
