//! Tests for the timed-reference merge pipeline.
//! Sibling-included from timed_reference_merge.rs.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::tier1::CandidateText;

fn timed_candidate(source: &str, lines: &[(&str, u64, u64)]) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: lines.iter().map(|(t, _, _)| (*t).to_string()).collect(),
        line_timings: Some(lines.iter().map(|(_, s, e)| (*s, *e)).collect()),
        has_timing: true,
    }
}

#[tokio::test]
async fn mode_b_short_circuit_emits_reference_lines_with_timed_merge_provenance() {
    let candidate = timed_candidate(
        "tier1:spotify",
        &[
            ("Amazing grace", 0, 3000),
            ("How sweet the sound", 3000, 6000),
        ],
    );
    let result = process(None, &candidate, 6000, None)
        .await
        .expect("Mode B must succeed for valid timed candidate");
    assert_eq!(result.provenance, "tier1:spotify+timed-merge");
    assert_eq!(result.lines.len(), 2);
    assert_eq!(result.lines[0].text, "Amazing grace");
    assert_eq!(result.lines[0].start_ms, 0);
    assert_eq!(result.lines[0].end_ms, 3000);
    assert!(
        result.lines[0].words.is_none(),
        "words: None per feedback_line_timing_only.md"
    );
    assert_eq!(result.lines[1].text, "How sweet the sound");
}

#[tokio::test]
async fn mode_b_returns_error_when_candidate_has_no_timings() {
    let candidate = CandidateText {
        source: "tier1:spotify".into(),
        lines: vec!["Amazing grace".into()],
        line_timings: None,
        has_timing: false,
    };
    let result = process(None, &candidate, 6000, None).await;
    assert!(matches!(result, Err(TimedMergeError::NoTimings)));
}

#[tokio::test]
async fn mode_b_returns_error_when_candidate_has_zero_lines() {
    let candidate = CandidateText {
        source: "tier1:spotify".into(),
        lines: vec![],
        line_timings: Some(vec![]),
        has_timing: true,
    };
    let result = process(None, &candidate, 6000, None).await;
    assert!(matches!(result, Err(TimedMergeError::EmptyReference)));
}

#[tokio::test]
async fn mode_a_with_asr_emits_reference_lines_and_timings() {
    // Mode A: ASR provided; reference timings authoritative; sanitize/phantom-filter/split apply.
    let asr = AlignedTrack {
        lines: vec![AlignedLine {
            text: "amazing grace".into(),
            start_ms: 0,
            end_ms: 3000,
            words: Some(vec![
                AlignedWord {
                    text: "amazing".into(),
                    start_ms: 0,
                    end_ms: 1500,
                    confidence: 0.9,
                },
                AlignedWord {
                    text: "grace".into(),
                    start_ms: 1500,
                    end_ms: 3000,
                    confidence: 0.9,
                },
            ]),
        }],
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    };
    let candidate = timed_candidate("lrclib", &[("Amazing grace", 0, 3000)]);
    let result = process(Some(&asr), &candidate, 3000, None)
        .await
        .expect("Mode A must succeed");
    assert_eq!(result.provenance, "lrclib+timed-merge");
    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.lines[0].text, "Amazing grace"); // reference text wins
    assert_eq!(result.lines[0].start_ms, 0);
    assert_eq!(result.lines[0].end_ms, 3000);
}

#[test]
fn candidate_to_aligned_lines_preserves_text_and_timings() {
    let cand = timed_candidate("lrclib", &[("alpha", 0, 1000), ("beta", 1500, 2500)]);
    let lines = candidate_to_aligned_lines(&cand);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "alpha");
    assert_eq!(lines[0].start_ms, 0);
    assert_eq!(lines[0].end_ms, 1000);
    assert!(lines[0].words.is_none());
    assert_eq!(lines[1].text, "beta");
    assert_eq!(lines[1].start_ms, 1500);
    assert_eq!(lines[1].end_ms, 2500);
}
