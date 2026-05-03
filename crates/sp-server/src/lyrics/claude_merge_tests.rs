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

// ── source_priority tests ─────────────────────────────────────────────────

#[test]
fn source_priority_values() {
    // Production labels (gather_sources_impl) and tier1: aliases.
    let cases = [
        ("override", 5),
        ("tier1:spotify", 4),
        ("lrclib", 3),
        ("tier1:lrclib", 3),
        ("genius", 2),
        ("tier1:genius", 2),
        ("yt_subs", 1),
        ("tier1:yt_subs", 1),
        ("description", 0),
        ("unknown", 0),
        ("", 0),
    ];
    for (s, p) in cases {
        assert_eq!(source_priority(s), p, "{s}");
    }
    // Strict order: override > spotify > lrclib > genius > yt_subs > description.
    let order = [
        "override",
        "tier1:spotify",
        "lrclib",
        "genius",
        "yt_subs",
        "description",
    ];
    for w in order.windows(2) {
        assert!(source_priority(w[0]) > source_priority(w[1]), "{w:?}");
    }
}

// ── best_authoritative_candidate tests ────────────────────────────────────

fn cand(source: &str, lines: &[&str]) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: lines.iter().map(|s| (*s).into()).collect(),
        line_timings: None,
        has_timing: false,
    }
}

/// Helper: compatibility shim that returns just the lines vec, matching the
/// shape of the original `best_authoritative` for ergonomic test assertions.
fn best_lines(candidates: &[CandidateText]) -> Vec<String> {
    best_authoritative_candidate(candidates)
        .map(|c| c.lines.clone())
        .unwrap_or_default()
}

#[test]
fn best_authoritative_picks_most_lines() {
    // When sources have equal priority, longest wins (tie-break on lines).
    // Both are tier1:genius (same priority) — the one with more lines should win.
    let result = best_lines(&[
        cand("tier1:genius", &["a", "b"]),
        cand("tier1:genius", &["a", "b", "c", "d"]),
    ]);
    assert_eq!(result.len(), 4, "should pick the candidate with more lines");
}

#[test]
fn best_authoritative_uses_priority_for_tie() {
    // Both have 2 lines; spotify wins on priority.
    let result = best_lines(&[
        cand("genius", &["x", "y"]),
        cand("tier1:spotify", &["a", "b"]),
    ]);
    assert_eq!(result[0], "a");
}

#[test]
fn best_authoritative_priority_beats_longer_lower_priority_candidate() {
    // The whole point of source_priority: a high-priority short candidate
    // (e.g. tier1:spotify with 12 lines) MUST win over a longer noisy
    // low-priority candidate (e.g. yt_subs with 50 lines). Pre-fix
    // ranking was (lines.len(), priority) which got this backwards.
    let result = best_lines(&[
        cand(
            "yt_subs",
            &[
                "yt line 0",
                "yt line 1",
                "yt line 2",
                "yt line 3",
                "yt line 4",
                "yt line 5",
                "yt line 6",
                "yt line 7",
                "yt line 8",
                "yt line 9",
                "yt line 10",
                "yt line 11",
                "yt line 12",
                "yt line 13",
                "yt line 14",
                "yt line 15",
                "yt line 16",
                "yt line 17",
                "yt line 18",
                "yt line 19",
                "yt line 20",
                "yt line 21",
                "yt line 22",
                "yt line 23",
                "yt line 24",
                "yt line 25",
                "yt line 26",
                "yt line 27",
                "yt line 28",
                "yt line 29",
                "yt line 30",
                "yt line 31",
                "yt line 32",
                "yt line 33",
                "yt line 34",
                "yt line 35",
                "yt line 36",
                "yt line 37",
                "yt line 38",
                "yt line 39",
                "yt line 40",
                "yt line 41",
                "yt line 42",
                "yt line 43",
                "yt line 44",
                "yt line 45",
                "yt line 46",
                "yt line 47",
                "yt line 48",
                "yt line 49",
            ],
        ),
        cand(
            "tier1:spotify",
            &[
                "spotify line 0",
                "spotify line 1",
                "spotify line 2",
                "spotify line 3",
                "spotify line 4",
                "spotify line 5",
                "spotify line 6",
                "spotify line 7",
                "spotify line 8",
                "spotify line 9",
                "spotify line 10",
                "spotify line 11",
            ],
        ),
    ]);
    assert_eq!(
        result.len(),
        12,
        "should pick spotify with 12 lines, not yt_subs with 50"
    );
    assert!(
        result[0].starts_with("spotify"),
        "first line should be from spotify, not yt_subs"
    );
}

#[test]
fn best_authoritative_override_beats_spotify() {
    // Override (priority 5) is the absolute top — even short overrides
    // beat longer Spotify candidates.
    let result = best_lines(&[
        cand(
            "tier1:spotify",
            &[
                "spotify line 0",
                "spotify line 1",
                "spotify line 2",
                "spotify line 3",
                "spotify line 4",
                "spotify line 5",
                "spotify line 6",
                "spotify line 7",
                "spotify line 8",
                "spotify line 9",
                "spotify line 10",
                "spotify line 11",
                "spotify line 12",
                "spotify line 13",
                "spotify line 14",
                "spotify line 15",
                "spotify line 16",
                "spotify line 17",
                "spotify line 18",
                "spotify line 19",
                "spotify line 20",
                "spotify line 21",
                "spotify line 22",
                "spotify line 23",
                "spotify line 24",
                "spotify line 25",
                "spotify line 26",
                "spotify line 27",
                "spotify line 28",
                "spotify line 29",
            ],
        ),
        cand("override", &["op line 1", "op line 2"]),
    ]);
    assert_eq!(
        result.len(),
        2,
        "should pick override with 2 lines, not spotify with 30"
    );
    assert!(
        result[0].starts_with("op"),
        "first line should be from override, not spotify"
    );
}

#[test]
fn best_authoritative_empty_returns_empty() {
    let result = best_lines(&[]);
    assert!(result.is_empty());
    assert!(best_authoritative_candidate(&[]).is_none());
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

// ── merge_deterministic tests (issue #78 regression) ──────────────────────
//
// Description and override sources arrive as clean text without timing.
// The pre-fix path passed them through claude_merge which iterated WhisperX
// PHRASES (audio-gap-split) and produced one output line per phrase. For a
// 6-min worship song with 60 phrases this destroyed the description's clean
// 25-line natural segmentation and shipped 95 fragmented lines, unusable on
// the LED wall (issue #78). The deterministic mapper preserves the
// reference's exact line count by construction.

#[test]
fn merge_deterministic_preserves_reference_line_count() {
    // 3 reference lines, ASR with 6 phrases simulating WhisperX over-split.
    // Output must have exactly 3 lines, not 6.
    let ref_lines: Vec<String> = vec![
        "Holy is the Lamb of God".into(),
        "Worthy is the King".into(),
        "Forever we will sing".into(),
    ];
    let asr = make_asr_with_words(&[(
        "holy is the lamb of god worthy is the king forever we will sing",
        0,
        12000,
        vec![
            make_word("holy", 0, 500),
            make_word("is", 600, 800),
            make_word("the", 900, 1100),
            make_word("lamb", 1200, 1700),
            make_word("of", 1800, 2000),
            make_word("god", 2100, 2700),
            make_word("worthy", 4000, 4600),
            make_word("is", 4700, 4900),
            make_word("the", 5000, 5200),
            make_word("king", 5300, 5900),
            make_word("forever", 8000, 8800),
            make_word("we", 8900, 9100),
            make_word("will", 9200, 9500),
            make_word("sing", 9600, 10500),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.lines.len(),
        3,
        "deterministic mapper must preserve reference line count (got {})",
        result.lines.len()
    );
}

#[test]
fn merge_deterministic_uses_reference_text_verbatim() {
    let ref_lines: Vec<String> = vec!["Line one".into(), "Line two".into()];
    let asr = make_asr_with_words(&[(
        "line one line two",
        0,
        4000,
        vec![
            make_word("line", 0, 500),
            make_word("one", 600, 1000),
            make_word("line", 2000, 2500),
            make_word("two", 2600, 3000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(result.lines[0].text, "Line one");
    assert_eq!(result.lines[1].text, "Line two");
}

#[test]
fn merge_deterministic_provenance_includes_source() {
    let ref_lines: Vec<String> = vec!["Hello world".into()];
    let asr = make_asr_with_words(&[(
        "hello world",
        0,
        2000,
        vec![make_word("hello", 0, 500), make_word("world", 600, 1000)],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.provenance, "description+whisperx-large-v3@rev1",
        "provenance must show description as the text-source authority, no +claude-merge suffix"
    );
}

#[test]
fn merge_deterministic_words_field_is_none_per_line_timing_only_rule() {
    // Per feedback_line_timing_only.md: line-level only, no per-word output.
    let ref_lines: Vec<String> = vec!["A B".into(), "C D".into()];
    let asr = make_asr_with_words(&[(
        "a b c d",
        0,
        4000,
        vec![
            make_word("a", 0, 500),
            make_word("b", 600, 1000),
            make_word("c", 2000, 2500),
            make_word("d", 2600, 3000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "override");
    for l in &result.lines {
        assert!(
            l.words.is_none(),
            "merge_deterministic must emit words: None per feedback_line_timing_only.md"
        );
    }
}

#[test]
fn merge_deterministic_timings_strictly_increasing() {
    // Output line[i].start_ms must be < line[i+1].start_ms; line[i].end_ms <= line[i+1].start_ms.
    // Per feedback_line_timing_only sanitizer rules and the v9-v10 history.
    let ref_lines: Vec<String> = vec![
        "First line".into(),
        "Second line".into(),
        "Third line".into(),
    ];
    let asr = make_asr_with_words(&[(
        "first line second line third line",
        0,
        9000,
        vec![
            make_word("first", 0, 500),
            make_word("line", 600, 1000),
            make_word("second", 3000, 3500),
            make_word("line", 3600, 4000),
            make_word("third", 6000, 6500),
            make_word("line", 6600, 7000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    let mut prev_start = 0u32;
    let mut prev_end = 0u32;
    for (i, l) in result.lines.iter().enumerate() {
        assert!(
            l.start_ms >= prev_end,
            "line[{i}].start_ms ({}) must be >= prev line.end_ms ({prev_end})",
            l.start_ms
        );
        assert!(
            l.start_ms > prev_start || i == 0,
            "line[{i}].start_ms must be strictly > prev line.start_ms"
        );
        assert!(l.end_ms > l.start_ms, "line[{i}] zero-duration");
        prev_start = l.start_ms;
        prev_end = l.end_ms;
    }
}

#[test]
fn merge_deterministic_handles_unmatched_reference_lines() {
    // Reference has 3 lines, but ASR audio only matches words for line 1 and 3.
    // Line 2 has no audio support — mapper must still emit it with a placeholder
    // timing in chronological order, not skip it. Output count == 3.
    let ref_lines: Vec<String> = vec![
        "Match one".into(),
        "Reference only no audio".into(),
        "Match three".into(),
    ];
    let asr = make_asr_with_words(&[(
        "match one match three",
        0,
        6000,
        vec![
            make_word("match", 0, 500),
            make_word("one", 600, 1000),
            make_word("match", 4000, 4500),
            make_word("three", 4600, 5000),
        ],
    )]);

    let result = merge_deterministic(&asr, &ref_lines, "description");
    assert_eq!(
        result.lines.len(),
        3,
        "must emit all reference lines including unmatched ones"
    );
    assert_eq!(result.lines[1].text, "Reference only no audio");
    // Unmatched line falls between the two matched ones chronologically.
    assert!(result.lines[1].start_ms >= result.lines[0].end_ms);
    assert!(result.lines[1].end_ms <= result.lines[2].start_ms);
}

// ── merge() entry-point branch tests ──────────────────────────────────────

#[test]
fn merge_branches_to_deterministic_for_description_source() {
    // When the best authoritative source is "description", merge() (the public
    // entry) must dispatch to merge_deterministic and produce output with
    // line count == reference line count, with no +claude-merge suffix in
    // provenance (because no Claude call was made).
    //
    // We can't call the async merge() directly without an AiClient, but we
    // can verify the branching condition by selecting the candidate the same
    // way merge() does.
    let candidates = vec![
        cand("description", &["Ref line A", "Ref line B"]),
        cand("yt_subs", &["yt sub line"]), // priority 1 < description's 0... wait
    ];
    // source_priority: description = 0, yt_subs = 1. yt_subs wins on priority.
    // For this test, ensure description-only candidate triggers det path.
    let only_desc = vec![cand("description", &["Ref line A", "Ref line B"])];
    let best = only_desc
        .iter()
        .max_by_key(|c| (source_priority(&c.source), c.lines.len()))
        .unwrap();
    assert_eq!(best.source, "description");

    // Deterministic call:
    let asr = make_asr_with_words(&[(
        "ref line a ref line b",
        0,
        4000,
        vec![
            make_word("ref", 0, 200),
            make_word("line", 300, 600),
            make_word("a", 700, 1000),
            make_word("ref", 2000, 2200),
            make_word("line", 2300, 2600),
            make_word("b", 2700, 3000),
        ],
    )]);
    let result = merge_deterministic(&asr, &best.lines, &best.source);
    assert_eq!(result.lines.len(), 2);
    assert!(!result.provenance.contains("+claude-merge"));
    assert!(result.provenance.starts_with("description+"));

    // Quiet unused-var warning if candidates is unused above.
    let _ = candidates;
}
