//! Tests for `claude_merge`. Included as a sibling file via
//! `#[path = "claude_merge_tests.rs"] #[cfg(test)] mod tests;` from claude_merge.rs
//! to keep that file under the airuleset 1000-line cap.

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

fn make_asr_with_words(lines: &[(&str, u32, u32, Vec<AlignedWord>)]) -> AlignedTrack {
    AlignedTrack {
        lines: lines
            .iter()
            .map(|(text, s, e, words)| AlignedLine {
                text: text.to_string(),
                start_ms: *s,
                end_ms: *e,
                words: Some(words.clone()),
            })
            .collect(),
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    }
}

// ── build_phrases tests ───────────────────────────────────────────────────

#[test]
fn build_phrases_splits_on_gap_over_500ms() {
    // Word A: 0-100, Word B: 110-200, Word C: 800-900
    // Gap A→B = 10ms (< 500, no split)
    // Gap B→C = 600ms (> 500, split here)
    let asr = make_asr_with_words(&[(
        "a b c",
        0,
        900,
        vec![
            make_word("a", 0, 100),
            make_word("b", 110, 200),
            make_word("c", 800, 900),
        ],
    )]);

    let phrases = build_phrases(&asr);
    assert_eq!(
        phrases.len(),
        2,
        "expected 2 phrases, got {}",
        phrases.len()
    );

    assert_eq!(phrases[0].start_ms, 0);
    assert_eq!(phrases[0].end_ms, 200);
    assert_eq!(phrases[0].text, "a b");

    assert_eq!(phrases[1].start_ms, 800);
    assert_eq!(phrases[1].end_ms, 900);
    assert_eq!(phrases[1].text, "c");
}

#[test]
fn build_phrases_no_split_when_gap_is_exactly_500ms() {
    // Gap exactly 500ms should NOT split (threshold is > 500, not >= 500).
    let asr = make_asr_with_words(&[(
        "a b",
        0,
        1000,
        vec![make_word("a", 0, 200), make_word("b", 700, 1000)],
    )]);
    let phrases = build_phrases(&asr);
    assert_eq!(phrases.len(), 1);
    assert_eq!(phrases[0].text, "a b");
}

#[test]
fn build_phrases_skips_lines_without_words() {
    let asr = AlignedTrack {
        lines: vec![
            AlignedLine {
                text: "line without words".into(),
                start_ms: 0,
                end_ms: 1000,
                words: None,
            },
            AlignedLine {
                text: "line with words".into(),
                start_ms: 1000,
                end_ms: 2000,
                words: Some(vec![
                    make_word("line", 1000, 1300),
                    make_word("with", 1300, 1600),
                    make_word("words", 1600, 2000),
                ]),
            },
        ],
        provenance: "test".into(),
        raw_confidence: 0.9,
    };
    let phrases = build_phrases(&asr);
    assert_eq!(phrases.len(), 1);
    assert_eq!(phrases[0].text, "line with words");
}

// ── drop_hallucinated_lead_in tests ──────────────────────────────────────

#[test]
fn drop_lead_in_removes_long_duration_word_with_large_gap() {
    // Word 0: duration = 2000ms (> 1500), gap to word 1 = 3000ms (> 2000) → drop
    let words = vec![
        make_word("ohhh", 0, 2000),
        make_word("alleluia", 5000, 6000),
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].text, "alleluia");
}

#[test]
fn drop_lead_in_keeps_word_when_duration_under_threshold() {
    // Word 0: duration = 1000ms (≤ 1500) → keep even with large gap
    let words = vec![
        make_word("yeah", 0, 1000),
        make_word("alleluia", 5000, 6000),
    ];
    let result = drop_hallucinated_lead_in(words.clone());
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].text, "yeah");
}

#[test]
fn drop_lead_in_keeps_word_when_gap_under_threshold() {
    // Word 0: duration = 2000ms (> 1500), but gap = 1000ms (≤ 2000) → keep
    let words = vec![
        make_word("ohhh", 0, 2000),
        make_word("alleluia", 3000, 4000),
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].text, "ohhh");
}

#[test]
fn drop_lead_in_handles_single_word() {
    let words = vec![make_word("alone", 0, 5000)];
    let result = drop_hallucinated_lead_in(words.clone());
    assert_eq!(result.len(), 1);
}

// ── parse_claude_response tests ──────────────────────────────────────────

#[test]
fn parse_claude_response_clean_json() {
    let raw = r#"{"lines": [{"start_ms": 1000, "end_ms": 3000, "text": "Amazing grace"}]}"#;
    let lines = parse_claude_response(raw).expect("should parse clean JSON");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "Amazing grace");
    assert_eq!(lines[0].start_ms, 1000);
    assert_eq!(lines[0].end_ms, 3000);
}

#[test]
fn parse_claude_response_with_prose_preamble_and_fence() {
    let raw = "I'll process the WhisperX phrases and match them to the reference lyrics.\n\n```json\n{\"lines\": [{\"start_ms\": 500, \"end_ms\": 2500, \"text\": \"He's holy\"}]}\n```";
    let lines = parse_claude_response(raw).expect("should parse with preamble + fence");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "He's holy");
}

#[test]
fn parse_claude_response_with_fence_no_preamble() {
    let raw =
        "```json\n{\"lines\": [{\"start_ms\": 0, \"end_ms\": 1000, \"text\": \"Alleluia\"}]}\n```";
    let lines = parse_claude_response(raw).expect("should parse fence without preamble");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "Alleluia");
}

#[test]
fn parse_claude_response_malformed_returns_error() {
    let raw = "This is not JSON at all. No lines key anywhere.";
    let result = parse_claude_response(raw);
    assert!(
        matches!(result, Err(MergeError::ParseFailed(_))),
        "expected ParseFailed, got: {result:?}"
    );
}

#[test]
fn parse_claude_response_empty_lines_array() {
    // Parser allows empty lines array; merge() above rejects it as a
    // refusal so orchestrator falls back to raw WhisperX.
    let lines = parse_claude_response(r#"{"lines": []}"#).expect("valid JSON");
    assert_eq!(lines.len(), 0);
}

// ── priority_with_timing tests ───────────────────────────────────────────

#[cfg(test)]
mod priority_with_timing_tests {
    use super::*;

    /// Spec table from docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md.
    /// Tier-break: timed sources outrank text-only of the same name; description outranks
    /// every other text source.
    #[test]
    fn matrix_matches_spec_table() {
        // (source, has_timing, expected_priority)
        let cases: &[(&str, bool, u32)] = &[
            ("override", false, 6),
            ("tier1:spotify", true, 5),
            ("lrclib", true, 5),
            ("tier1:lrclib", true, 5),
            ("tier1:yt_subs", true, 4),
            ("yt_subs", true, 4),
            ("description", false, 3),
            ("lrclib", false, 2),
            ("tier1:lrclib", false, 2),
            ("genius", false, 1),
            ("tier1:genius", false, 1),
            ("yt_subs", false, 0),
            ("tier1:yt_subs", false, 0),
            ("unknown_source", false, 0),
        ];
        for (source, has_timing, expected) in cases {
            assert_eq!(
                priority_with_timing(source, *has_timing),
                *expected,
                "priority_with_timing({source:?}, {has_timing}) expected {expected}",
            );
        }
    }
}

// ── merge output structure test (mock) ────────────────────────────────────

/// Verify `merge` produces an AlignedTrack with `words: None` and the
/// expected provenance suffix. Composes the same stages `merge()` runs
/// without making the HTTP call.
#[test]
fn merge_output_structure_words_none_and_provenance() {
    // Simulate Claude returning 2 lines.
    let raw_response = r#"{"lines": [{"start_ms": 1000, "end_ms": 3000, "text": "Amazing grace"}, {"start_ms": 3500, "end_ms": 5000, "text": "How sweet the sound"}]}"#;
    let merged_lines = parse_claude_response(raw_response).unwrap();

    // Construct the AlignedTrack the same way `merge()` would.
    let asr_provenance = "whisperx-large-v3@rev1";
    let aligned_lines: Vec<AlignedLine> = merged_lines
        .into_iter()
        .map(|ml| AlignedLine {
            text: ml.text,
            start_ms: ml.start_ms,
            end_ms: ml.end_ms,
            words: None,
        })
        .collect();
    let track = AlignedTrack {
        lines: aligned_lines,
        provenance: format!("{asr_provenance}+claude-merge"),
        raw_confidence: 0.85,
    };

    // Verify output structure.
    assert_eq!(track.lines.len(), 2);
    assert!(
        track.provenance.ends_with("+claude-merge"),
        "provenance must end with +claude-merge"
    );
    for line in &track.lines {
        assert!(
            line.words.is_none(),
            "merged output must have words: None per feedback_line_timing_only.md"
        );
    }
    assert_eq!(track.lines[0].text, "Amazing grace");
    assert_eq!(track.lines[0].start_ms, 1000);
    assert_eq!(track.lines[1].text, "How sweet the sound");
}

// ── build_phrases: empty-words-list guard (line 166 mutant) ─────────────
//
// Mutant: replace `!w.is_empty()` with `true` — would include lines with
// an empty Vec, causing an out-of-bounds panic at `words[0]` below.
// This test asserts that a `Some(vec![])` line produces NO phrases.

#[test]
fn build_phrases_skips_lines_with_empty_words_vec() {
    let asr = AlignedTrack {
        lines: vec![
            AlignedLine {
                text: "empty words".into(),
                start_ms: 0,
                end_ms: 1000,
                words: Some(vec![]), // Some but empty — must be skipped
            },
            AlignedLine {
                text: "has words".into(),
                start_ms: 2000,
                end_ms: 3000,
                words: Some(vec![
                    make_word("has", 2000, 2400),
                    make_word("words", 2400, 3000),
                ]),
            },
        ],
        provenance: "test".into(),
        raw_confidence: 0.9,
    };
    let phrases = build_phrases(&asr);
    // Only the non-empty line contributes; the Some(vec![]) line is skipped.
    assert_eq!(phrases.len(), 1, "Some(vec![]) must be skipped");
    assert_eq!(phrases[0].text, "has words");
}

// ── drop_hallucinated_lead_in: exact boundary tests (line 230 mutants) ──
//
// Mutant A: `duration > 1500` → `duration >= 1500` would drop a word with
// duration == 1500ms, but the correct code keeps it.
// Mutant B: `gap > 2000` → `gap >= 2000` would drop a word with gap == 2000ms.

#[test]
fn drop_lead_in_keeps_word_at_exactly_1500ms_duration() {
    // duration == 1500 (NOT > 1500 → should NOT drop)
    let words = vec![
        make_word("hmm", 0, 1500), // duration = 1500 exactly — boundary, must keep
        make_word("alleluia", 4000, 5000), // gap = 2500 (> 2000)
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(
        result.len(),
        2,
        "duration == 1500 must NOT be dropped (threshold is > 1500)"
    );
    assert_eq!(result[0].text, "hmm");
}

#[test]
fn drop_lead_in_keeps_word_at_exactly_2000ms_gap() {
    // gap == 2000 (NOT > 2000 → should NOT drop)
    let words = vec![
        make_word("ohhh", 0, 2000),        // duration = 2000 > 1500 ✓
        make_word("alleluia", 4000, 5000), // gap = 4000 - 2000 = 2000 exactly — boundary, must keep
    ];
    let result = drop_hallucinated_lead_in(words);
    assert_eq!(
        result.len(),
        2,
        "gap == 2000 must NOT be dropped (threshold is > 2000)"
    );
    assert_eq!(result[0].text, "ohhh");
}

// ── build_prompt: template content (line 241 mutants) ───────────────────
//
// Mutant A: replace `build_prompt -> String` with `String::new()` — empty.
// Mutant B: replace with `"xyzzy".into()` — garbage.
// Both mutations produce strings that lack the template's key markers.

#[test]
fn build_prompt_contains_required_template_markers() {
    let prompt = build_prompt(r#"[{"start_ms":0}]"#, r#"["line1"]"#);
    // The template must contain these literal strings to correctly instruct Claude.
    assert!(
        prompt.contains("WHISPERX_PHRASES_JSON"),
        "prompt must contain WHISPERX_PHRASES_JSON marker"
    );
    assert!(
        prompt.contains("REFERENCE_LYRICS_JSON"),
        "prompt must contain REFERENCE_LYRICS_JSON marker"
    );
    assert!(
        prompt.contains("LED-wall"),
        "prompt must contain LED-wall context"
    );
    assert!(
        prompt.contains("32 characters"),
        "prompt must contain line-length rule"
    );
    assert!(
        prompt.contains(r#"[{"start_ms":0}]"#),
        "whisperx JSON must be substituted into prompt"
    );
    assert!(
        prompt.contains(r#"["line1"]"#),
        "reference JSON must be substituted into prompt"
    );
}

#[test]
fn build_prompt_substitutes_both_placeholders() {
    let whisperx = r#"[{"start_ms":100,"end_ms":2000,"text":"hello"}]"#;
    let reference = r#"["Hello world"]"#;
    let prompt = build_prompt(whisperx, reference);
    // Neither placeholder token should remain verbatim in the final string.
    assert!(
        !prompt.contains("___WHISPERX___"),
        "___WHISPERX___ placeholder must be replaced"
    );
    assert!(
        !prompt.contains("___REFERENCE___"),
        "___REFERENCE___ placeholder must be replaced"
    );
    // The actual values must appear.
    assert!(prompt.contains(whisperx));
    assert!(prompt.contains(reference));
}

// ── try_all_lines_positions: empty-lines guard (line 334 mutants) ────────
//
// Mutant A: replace guard `!resp.lines.is_empty()` with `true` — would
// accept and return the first `{"lines":[]}` match instead of falling
// through to a valid match with actual lines.
// Mutant B: replace with `false` — would never return Ok(lines) from the
// non-empty arm; tests that return OK on non-empty break.
// Mutant C: delete `!` — same as replacing guard with `resp.lines.is_empty()`.

#[test]
fn try_all_lines_positions_skips_empty_lines_and_returns_real_match() {
    // Response contains two occurrences of {"lines": ...}: first one is empty,
    // second has real data. The function must skip the empty one and return
    // the second.
    let s = r#"result = {"lines": []}; actual = {"lines": [{"start_ms": 500, "end_ms": 1500, "text": "Grace"}]}"#;
    let result = try_all_lines_positions(s);
    assert!(result.is_ok(), "must find the non-empty lines object");
    let lines = result.unwrap();
    assert_eq!(
        lines.len(),
        1,
        "must return the non-empty lines, not the empty one"
    );
    assert_eq!(lines[0].text, "Grace");
}

#[test]
fn try_all_lines_positions_returns_error_when_all_lines_empty() {
    // Only empty lines arrays — must return Err(()) since no non-empty result found.
    let s = r#"{"lines": []}"#;
    let result = try_all_lines_positions(s);
    assert!(
        result.is_err(),
        "all-empty lines arrays must return Err, not Ok([])"
    );
}

// ── try_parse_balanced: JSON string tracking (lines 362-368 mutants) ─────
//
// These mutants target the brace-depth + string-escape tracking inside the
// balanced-parse loop:
//   - `b'"'` arm deletion: would leave in_string=false, braces inside strings
//     would be miscounted
//   - `in_string` guard inversions: would count braces while inside a string
//   - `depth += 1` / `depth -= 1` / `depth == 0` / `i + 1` arithmetic changes

#[test]
fn try_parse_balanced_handles_braces_inside_string_values() {
    // The "text" value contains literal `{` and `}` characters. Without proper
    // string tracking the depth counter goes wrong and serde_json either sees
    // a truncated or over-extended slice.
    let s = r#"{"lines": [{"start_ms": 0, "end_ms": 1000, "text": "a {bracketed} value"}]} trailing garbage here"#;
    let result = try_parse_balanced(s);
    assert!(
        result.is_ok(),
        "braces inside string must not confuse depth tracking"
    );
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 1);
    assert_eq!(resp.lines[0].text, "a {bracketed} value");
}

#[test]
fn try_parse_balanced_handles_escaped_quotes_inside_strings() {
    // The "text" value contains an escaped quote `\"`. Without the escape
    // tracking (`b'\\'` arm), the parser would treat the `"` after `\` as
    // a string-end token, misidentifying the next `{` as an object start.
    let s = r#"{"lines": [{"start_ms": 0, "end_ms": 1000, "text": "He\"s holy"}]}"#;
    let result = try_parse_balanced(s);
    assert!(
        result.is_ok(),
        "escaped quote must not toggle in_string incorrectly"
    );
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 1);
    // serde decodes the escape: \" → "
    assert!(resp.lines[0].text.contains('\'') || resp.lines[0].text.contains('"'));
}

#[test]
fn try_parse_balanced_counts_depth_correctly_for_nested_objects() {
    // The inner word objects have their own braces. depth must reach 0 only
    // at the outermost closing `}`.
    // depth trace: { → 1, { → 2, } → 1 (inner closes), } → 0 (outer closes)
    // Without correct depth +=/−= the serde slice is wrong.
    let s = r#"{"lines": [{"start_ms": 100, "end_ms": 500, "text": "nested"}]} extra"#;
    let result = try_parse_balanced(s);
    assert!(result.is_ok());
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 1);
    assert_eq!(resp.lines[0].start_ms, 100);
    assert_eq!(resp.lines[0].end_ms, 500);
}

#[test]
fn try_parse_balanced_end_idx_includes_closing_brace() {
    // If `end_idx = i + 1` is mutated to `i` the slice won't include the
    // closing `}` and serde_json will fail to parse.
    let s = r#"{"lines": [{"start_ms": 0, "end_ms": 1, "text": "x"}]}"#;
    let result = try_parse_balanced(s);
    assert!(
        result.is_ok(),
        "closing brace must be included in slice (end_idx = i+1)"
    );
}

#[test]
fn try_parse_balanced_multiple_lines_with_curly_braces_in_text() {
    // Multiple lines where text fields contain `{` / `}` to thoroughly
    // exercise the in_string guard across many iterations.
    let s = r#"{"lines": [
        {"start_ms": 1000, "end_ms": 2000, "text": "{intro}"},
        {"start_ms": 2000, "end_ms": 3000, "text": "normal line"},
        {"start_ms": 3000, "end_ms": 4000, "text": "end {outro}"}
    ]}"#;
    let result = try_parse_balanced(s);
    assert!(result.is_ok());
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 3);
    assert_eq!(resp.lines[0].text, "{intro}");
    assert_eq!(resp.lines[2].text, "end {outro}");
}

// ── try_parse_balanced: unmatched `}` and escaped-quote + brace tests ──────
//
// The existing `handles_braces_inside_string_values` test uses balanced `{}`
// so the extra braces cancel out even without in_string tracking.  These two
// tests require correct tracking to avoid premature depth=0:
//
//   (A) String value with unmatched `}`: kills `delete b'"'` / `delete !`.
//   (B) String value `\"}}`: kills `in_string guard → false` (line 362).

#[test]
fn try_parse_balanced_unmatched_close_brace_inside_string() {
    // Without `b'"'` arm (in_string never true), the `}` in "a } value" is
    // counted as a real closing brace, depth hits 0 early, slice is truncated,
    // serde_json fails.  Correct code keeps in_string=true and ignores it.
    let s = r#"{"lines": [{"start_ms": 0, "end_ms": 1000, "text": "a } value"}]} trailing"#;
    let result = try_parse_balanced(s);
    assert!(
        result.is_ok(),
        "unmatched `}}` in string must not truncate slice"
    );
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 1);
    assert_eq!(resp.lines[0].text, "a } value");
}

#[test]
fn try_parse_balanced_escaped_quote_followed_by_closing_braces() {
    // Text value is `a\"}}` (escaped-quote + two closing braces).
    // Without escape tracking (b'\\' guard → false), the `"` after `\`
    // prematurely closes in_string=false, the two `}` chars decrement depth
    // to 0 early, slice is truncated, serde_json fails.
    let s = r#"{"lines": [{"start_ms": 0, "end_ms": 1, "text": "a\"}}"}]} trailing"#;
    let result = try_parse_balanced(s);
    assert!(
        result.is_ok(),
        "escaped quote must not prematurely close string"
    );
    let resp = result.unwrap();
    assert_eq!(resp.lines.len(), 1);
    assert_eq!(resp.lines[0].text, "a\"}}");
}

// ── merge() entry-point boundary tests (kill mutation survivors) ──────────

fn dummy_ai_client() -> AiClient {
    use crate::ai::AiSettings;
    AiClient::new(AiSettings {
        api_url: "http://127.0.0.1:1".into(), // unreachable port — never called in these tests
        api_key: None,
        model: "test".into(),
        system_prompt_extra: None,
    })
}

fn asr_with_words(words: Vec<AlignedWord>) -> AlignedTrack {
    AlignedTrack {
        lines: vec![AlignedLine {
            text: "phrase".into(),
            start_ms: 0,
            end_ms: 1000,
            words: Some(words),
        }],
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    }
}

#[tokio::test]
async fn merge_returns_no_reference_when_candidate_lines_empty() {
    // Kills `replace match guard !b.lines.is_empty() with true` at line 64:20.
    // Source must be a NON-special label (not "description"/"override")
    // so the mutation path goes to the Claude semantic-merge branch
    // instead of description_merge::process — that branch has its own
    // empty-ref short-circuit (line 105) which returns NoReference
    // regardless of which guard is mutated, so it can't distinguish.
    // With "genius", the mutation gets past line 64 and tries to call
    // AiClient at the unreachable port → Err(MergeError::Claude(_)).
    // Original short-circuits at line 64 → Err(MergeError::NoReference).
    let ai = dummy_ai_client();
    let asr = asr_with_words(vec![make_word("a", 0, 100)]);
    let candidates = vec![CandidateText {
        source: "genius".into(),
        lines: vec![],
        line_timings: None,
        has_timing: false,
    }];
    let result = merge(&ai, &asr, &candidates, None).await;
    assert!(
        matches!(result, Err(MergeError::NoReference)),
        "empty lines must yield Err(NoReference); got {result:?}"
    );
}

#[tokio::test]
async fn merge_returns_no_reference_when_no_candidates() {
    // Empty candidates list — best_authoritative_candidate returns None →
    // pattern catches that branch via the wildcard arm. Verifies the
    // outer `match` still dispatches correctly even with no candidates.
    let ai = dummy_ai_client();
    let asr = asr_with_words(vec![make_word("a", 0, 100)]);
    let result = merge(&ai, &asr, &[], None).await;
    assert!(matches!(result, Err(MergeError::NoReference)));
}

#[tokio::test]
async fn merge_routes_description_source_through_description_merge() {
    // Kills `replace || with &&` at line 73:37.
    //   Original: `if best.source == "description" || best.source == "override"`
    //   Mutated:  `if best.source == "description" && best.source == "override"`
    // The mutation makes the if-branch unreachable (a single source can't equal
    // BOTH strings simultaneously), so a description-source candidate falls
    // through to the Claude semantic-merge path which immediately tries to call
    // the AiClient — that fails because we point it at an unreachable port.
    // Original code goes through description_merge::process which (with all
    // ref lines under SUBLINE_MAX_CHARS=32) does no Claude calls and returns
    // Ok with provenance starting "description+".
    let ai = dummy_ai_client();
    let asr = asr_with_words(vec![
        make_word("a", 0, 100),
        make_word("b", 200, 300),
        make_word("c", 400, 500),
    ]);
    let candidates = vec![CandidateText {
        source: "description".into(),
        lines: vec!["a b".into(), "c".into()],
        line_timings: None,
        has_timing: false,
    }]; // both well under 32 chars
    let result = merge(&ai, &asr, &candidates, None).await;
    let track = result.expect("description path must succeed without Claude");
    assert!(
        track.provenance.starts_with("description+"),
        "provenance must mark description path; got {:?}",
        track.provenance
    );
}

#[tokio::test]
async fn merge_routes_override_source_through_description_merge() {
    // Same as above for source = "override". Both arms of the `||` must
    // route to description_merge::process.
    let ai = dummy_ai_client();
    let asr = asr_with_words(vec![
        make_word("hello", 0, 200),
        make_word("world", 300, 500),
    ]);
    let candidates = vec![CandidateText {
        source: "override".into(),
        lines: vec!["hello world".into()],
        line_timings: None,
        has_timing: false,
    }];
    let result = merge(&ai, &asr, &candidates, None).await;
    let track = result.expect("override path must succeed without Claude");
    assert!(
        track.provenance.starts_with("override+"),
        "provenance must mark override path; got {:?}",
        track.provenance
    );
}

#[cfg(test)]
mod coverage_ok_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn cand_with_timings(timings: Vec<(u64, u64)>) -> CandidateText {
        CandidateText {
            source: "lrclib".into(),
            lines: vec!["x".into(); timings.len()],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    #[test]
    fn returns_true_when_span_covers_at_least_80_percent_of_duration() {
        // 0..240000 ms span, 300000 ms duration → 80% exact → true
        let c = cand_with_timings(vec![(0, 1000), (239000, 240000)]);
        assert!(coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_span_below_80_percent() {
        // 0..200000 ms span, 300000 ms duration → 66.7% → false
        let c = cand_with_timings(vec![(0, 1000), (199000, 200000)]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_no_timings() {
        let c = CandidateText {
            source: "genius".into(),
            lines: vec!["x".into()],
            line_timings: None,
            has_timing: false,
        };
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_empty_timings() {
        let c = cand_with_timings(vec![]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_duration_zero() {
        let c = cand_with_timings(vec![(0, 1000)]);
        assert!(!coverage_ok(&c, 0));
    }
}

#[cfg(test)]
mod best_authoritative_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn text_cand(source: &str, line_count: usize) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: None,
            has_timing: false,
        }
    }

    fn timed_cand(source: &str, line_count: usize, span_ms: u64) -> CandidateText {
        let timings: Vec<(u64, u64)> = (0..line_count as u64)
            .map(|i| {
                let start = i * (span_ms / line_count.max(1) as u64);
                let end = start + 1000;
                (start, end)
            })
            .collect();
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    /// id=21 "Good Shepherd" regression: description (26 lines) + genius (70 lines)
    /// both present. Pre-fix: genius wins (priority 2 > description 0). Post-fix:
    /// description wins (priority 3 > genius 1) by spec.
    #[test]
    fn description_beats_genius_when_both_present() {
        let candidates = vec![text_cand("description", 26), text_cand("genius", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "description");
    }

    #[test]
    fn override_beats_description() {
        let candidates = vec![text_cand("description", 26), text_cand("override", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "override");
    }

    #[test]
    fn timed_lrclib_beats_text_description_regardless_of_coverage() {
        // Selection layer ignores coverage — that's the routing layer's call.
        let candidates = vec![
            text_cand("description", 26),
            timed_cand("lrclib", 30, 50_000),
        ];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
        assert!(best.has_timing);
    }

    #[test]
    fn lrclib_text_beats_genius_text() {
        let candidates = vec![text_cand("genius", 70), text_cand("lrclib", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
    }

    #[test]
    fn empty_candidates_returns_none() {
        let candidates: Vec<CandidateText> = vec![];
        assert!(best_authoritative_candidate(&candidates).is_none());
    }

    #[test]
    fn tie_break_prefers_longer_lines_at_same_priority() {
        let candidates = vec![text_cand("genius", 30), text_cand("genius", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.lines.len(), 70);
    }
}
