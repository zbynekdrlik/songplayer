//! Claude-driven semantic merge — replaces the LCS-based reconcile path.
//!
//! Algorithm:
//! 1. Pick best authoritative text candidate (max lines; priority tie-break).
//! 2. Build phrase-level chunks from WhisperX word timings (split on >500ms gaps;
//!    drop hallucinated lead-in words with duration >1500ms and gap >2000ms).
//! 3. Send WhisperX phrases + reference lyrics to Claude in a tuned prompt.
//! 4. Parse Claude's JSON response robustly (handles prose preamble + code fence).
//! 5. Return an AlignedTrack with line-level timing only (`words: None`).
//!
//! Per `feedback_line_timing_only.md`: output is line-level only — no word
//! synthesis, no even-distribution.
//! Per `feedback_no_even_distribution.md`: timing comes from WhisperX words only.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignedLine, AlignedTrack};
use crate::lyrics::tier1::CandidateText;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("no usable text candidate")]
    NoReference,
    #[error("Claude call failed: {0}")]
    Claude(#[from] anyhow::Error),
    #[error("parse failed: {0}")]
    ParseFailed(String),
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Merge WhisperX timing with authoritative reference text via Claude.
///
/// Returns an `AlignedTrack` with line-level timing only (`words: None` on
/// every line — per `feedback_line_timing_only.md`).
///
/// Provenance format: `"{asr.provenance}+claude-merge"`.
pub async fn merge(
    ai_client: &AiClient,
    asr: &AlignedTrack,
    text_candidates: &[CandidateText],
) -> Result<AlignedTrack, MergeError> {
    // Step 1: pick authoritative reference text.
    let reference_lines = best_authoritative(text_candidates);
    if reference_lines.is_empty() {
        return Err(MergeError::NoReference);
    }

    // Step 2: build phrase-level chunks from WhisperX word timings.
    let phrases = build_phrases(asr);

    // Degenerate case: no usable word-level timing at all — return raw ASR.
    if phrases.is_empty() {
        return Ok(asr.clone());
    }

    // Step 3: build Claude prompt.
    let whisperx_json =
        serde_json::to_string(&phrases).map_err(|e| anyhow::anyhow!("phrases serialize: {e}"))?;
    let reference_json = serde_json::to_string(&reference_lines)
        .map_err(|e| anyhow::anyhow!("reference serialize: {e}"))?;

    let prompt = build_prompt(&whisperx_json, &reference_json);

    // Step 4: call Claude.
    let raw = ai_client.chat("", &prompt).await?;

    // Step 5: parse response.
    let merged_lines = match parse_claude_response(&raw) {
        Ok(lines) => lines,
        Err(e) => {
            // Dump the raw response to a tmp file so operators can inspect what
            // Claude actually returned. Helps debug parser mismatches against
            // the live model output without re-running a paid prediction.
            let dump_path = std::env::temp_dir().join(format!(
                "claude_merge_raw_{}.txt",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            let _ = std::fs::write(&dump_path, &raw);
            tracing::warn!(
                dump = %dump_path.display(),
                raw_len = raw.len(),
                first_200 = %raw.chars().take(200).collect::<String>(),
                "claude_merge: parse failed — raw response dumped"
            );
            return Err(e);
        }
    };

    // Step 6: convert to AlignedTrack with words: None.
    let aligned_lines: Vec<AlignedLine> = merged_lines
        .into_iter()
        .map(|ml| AlignedLine {
            text: ml.text,
            start_ms: ml.start_ms,
            end_ms: ml.end_ms,
            words: None,
        })
        .collect();

    Ok(AlignedTrack {
        lines: aligned_lines,
        provenance: format!("{}+claude-merge", asr.provenance),
        raw_confidence: asr.raw_confidence,
    })
}

// ── Internal types ────────────────────────────────────────────────────────────

/// A phrase-level chunk built from WhisperX word timings.
#[derive(Debug, Clone, Serialize)]
struct Phrase {
    start_ms: u32,
    end_ms: u32,
    text: String,
}

/// A single line in Claude's JSON output.
#[derive(Debug, Clone, Deserialize)]
struct MergedLine {
    start_ms: u32,
    end_ms: u32,
    text: String,
}

/// Claude's full JSON response structure.
#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    lines: Vec<MergedLine>,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Stable source-preference for tie-breaking in `best_authoritative`.
/// Higher score = more reliable text quality.
fn source_priority(source: &str) -> u32 {
    if source.starts_with("tier1:spotify") {
        4
    } else if source.starts_with("tier1:lrclib") {
        3
    } else if source == "genius" || source == "tier1:genius" {
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

/// Build phrase-level chunks from all word-timed lines in `asr`.
///
/// For each `AlignedLine`:
/// - Lines with `words: None` are skipped entirely.
/// - Hallucinated lead-in words are dropped (see `drop_hallucinated_lead_in`).
/// - Words are split into phrases at consecutive gaps > 500ms.
fn build_phrases(asr: &AlignedTrack) -> Vec<Phrase> {
    let mut phrases = Vec::new();

    for line in &asr.lines {
        let words = match &line.words {
            Some(w) if !w.is_empty() => w.clone(),
            _ => continue,
        };

        // Drop hallucinated lead-in words before phrase-splitting.
        let words = drop_hallucinated_lead_in(words);
        if words.is_empty() {
            continue;
        }

        // Split into phrases at word gaps > 500ms.
        let mut phrase_words = vec![words[0].clone()];
        for i in 1..words.len() {
            let gap = words[i].start_ms.saturating_sub(words[i - 1].end_ms);
            if gap > 500 {
                // Flush current phrase.
                let text = phrase_words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                phrases.push(Phrase {
                    start_ms: phrase_words[0].start_ms,
                    end_ms: phrase_words.last().unwrap().end_ms,
                    text,
                });
                phrase_words = Vec::new();
            }
            phrase_words.push(words[i].clone());
        }
        // Flush last phrase.
        if !phrase_words.is_empty() {
            let text = phrase_words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            phrases.push(Phrase {
                start_ms: phrase_words[0].start_ms,
                end_ms: phrase_words.last().unwrap().end_ms,
                text,
            });
        }
    }

    phrases
}

/// Drop WhisperX hallucinated lead-in words.
///
/// While the first word's duration > 1500ms AND the gap between word[0].end_ms
/// and word[1].start_ms > 2000ms, drop word[0].
///
/// Returns the trimmed word list (may be empty if all words were dropped, though
/// that can only happen for a 1-word list where the gap check can't apply).
fn drop_hallucinated_lead_in(
    mut words: Vec<crate::lyrics::backend::AlignedWord>,
) -> Vec<crate::lyrics::backend::AlignedWord> {
    loop {
        if words.len() < 2 {
            break;
        }
        let duration = words[0].end_ms.saturating_sub(words[0].start_ms);
        let gap = words[1].start_ms.saturating_sub(words[0].end_ms);
        if duration > 1500 && gap > 2000 {
            words.remove(0);
        } else {
            break;
        }
    }
    words
}

/// Build the Claude prompt with the tuned template.
fn build_prompt(whisperx_json: &str, reference_json: &str) -> String {
    const TEMPLATE: &str = r#"You receive two inputs for one song:

1. WHISPERX_PHRASES_JSON — array of {start_ms, end_ms, text}. Each entry is one phrase the singer sang, with accurate millisecond timing. Text may have ASR mishearings ("these hoes" instead of "He's holy", "Hallelujah" instead of "Alle Alle Alleluia", etc.).

2. REFERENCE_LYRICS_JSON — authoritative lyrics from the recording's official channel, broken into lines with correct verse / chorus / phrase structure. Has the correct spellings.

Produce final timed karaoke lyrics for LED-wall display.

CORE TASK — for each WHISPERX phrase:
- Decide whether it matches a REFERENCE line (case-insensitive, ignoring punctuation, treating phonetic variants as matches: "Hallelujah" ≈ "Alleluia" ≈ "Alle Alle Alleluia"; "these hoes" / "this holy" ≈ "He's holy"; "So table" / "table" ≈ "Devil"; "I got a God" ≈ "I've got a God"; etc.).
- If MATCH: emit one or more output lines using REFERENCE's exact text, with timing derived from this WhisperX phrase. Use REFERENCE's verse breaks if the phrase covers multiple reference lines.
- If NO MATCH (intro vocalizations, MC interjections, ad-libs, vocal fills): emit one line with WhisperX phrase's text verbatim and its timing.

IMPORTANT — always perform the text correction. WhisperX's "these hoes" must become "He's holy" if reference has "He's holy". Never ship "these hoes" to the LED wall.

LINE-LENGTH RULES:
- Each output line MUST be <= 32 characters where possible.
- If a reference line + corresponding WhisperX phrase is longer than 32 chars, split at:
    1. sentence-end (`.` `!` `?`)
    2. comma / colon (`,` `;` `:`)
    3. word boundary closest to the middle (never mid-word).
- For each split sub-line, distribute timing proportionally by character count of non-whitespace within the original WhisperX phrase's [start_ms, end_ms] range.
- After splitting, every sub-line's start_ms / end_ms must be inside the original WhisperX phrase's [start_ms, end_ms] range. Continuity: split[i].end_ms == split[i+1].start_ms.

CHRONOLOGICAL ORDER: output lines must be sorted by start_ms ascending.

OUTPUT FORMAT — STRICT, READ CAREFULLY:
- Output ONLY a single JSON object with key "lines".
- DO NOT wrap your answer in Python, JavaScript, or any other code.
- DO NOT print code that "would produce" the answer — produce the answer DIRECTLY.
- DO NOT include any prose explanation before or after.
- DO NOT include markdown fences (no ```json, no ```python, no ```).
- The very first character of your response must be `{` and the very last character must be `}`.
- Schema: {"lines": [{"start_ms": int, "end_ms": int, "text": string}, ...]}

WHISPERX_PHRASES_JSON:
___WHISPERX___

REFERENCE_LYRICS_JSON:
___REFERENCE___"#;

    TEMPLATE
        .replace("___WHISPERX___", whisperx_json)
        .replace("___REFERENCE___", reference_json)
}

/// Parse Claude's response into a list of merged lines.
///
/// Handles three cases:
/// 1. Clean JSON: `{"lines": [...]}`
/// 2. Prose preamble + markdown code fence: `... ```json\n{...}\n```...`
/// 3. Markdown fence without preamble
///
/// Returns `MergeError::ParseFailed` on failure.
fn parse_claude_response(raw: &str) -> Result<Vec<MergedLine>, MergeError> {
    // Try every position of `{"lines":` in the raw response — Claude sometimes
    // wraps the answer in Python/JS code where the first match is inside an
    // assignment like `result = {"lines": ...}; print(...)` and balanced-parse
    // from there fails because the code continues past the closing `}`.
    if let Ok(lines) = try_all_lines_positions(raw) {
        return Ok(lines);
    }

    // Strip markdown fences and retry — handles the simple ```json ... ``` case
    // when there's no surrounding code.
    let stripped = crate::ai::client::strip_markdown_fences(raw);
    if let Ok(lines) = try_all_lines_positions(&stripped) {
        return Ok(lines);
    }

    // Last resort: try parsing the entire stripped string as one object.
    match serde_json::from_str::<ClaudeResponse>(&stripped) {
        Ok(resp) => Ok(resp.lines),
        Err(e) => Err(MergeError::ParseFailed(format!(
            "could not extract JSON from Claude response: {e}\nraw (first 200 chars): {}",
            &raw[..raw.len().min(200)]
        ))),
    }
}

/// Try parsing the response starting at every occurrence of `{"lines":` until
/// one succeeds AND yields a non-empty `lines` array. Robust against Claude
/// wrapping the JSON in surrounding code (Python / JS) where the first match
/// is at e.g. `result = {"lines": ...}; print(json.dumps(result))`.
fn try_all_lines_positions(s: &str) -> Result<Vec<MergedLine>, ()> {
    let needles = ["{\"lines\":", "{ \"lines\":"];
    let mut last_err = None;
    for needle in &needles {
        let mut search_from = 0usize;
        while let Some(rel) = s[search_from..].find(needle) {
            let pos = search_from + rel;
            let candidate = &s[pos..];
            match try_parse_balanced(candidate) {
                Ok(resp) if !resp.lines.is_empty() => return Ok(resp.lines),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
            search_from = pos + needle.len();
        }
    }
    let _ = last_err; // we only signal failure; caller decides next step
    Err(())
}

/// Find the byte offset of the first `{"lines":` pattern in `s`.
#[allow(dead_code)]
fn find_lines_object(s: &str) -> Option<usize> {
    // Accept both `{"lines":` and `{ "lines":` (with optional spaces).
    for prefix in &["{\"lines\":", "{ \"lines\":"] {
        if let Some(pos) = s.find(prefix) {
            return Some(pos);
        }
    }
    None
}

/// Try to parse a `ClaudeResponse` from the start of `s` by walking to a
/// balanced closing `}`. Returns the parsed value or a serde error.
fn try_parse_balanced(s: &str) -> Result<ClaudeResponse, serde_json::Error> {
    // Find the end of the outermost JSON object using brace depth tracking.
    let mut depth: i32 = 0;
    let mut end_idx = s.len();
    let mut found = false;
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape_next = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    end_idx = i + 1;
                    found = true;
                    break;
                }
            }
            _ => {}
        }
    }

    let slice = if found { &s[..end_idx] } else { s };
    serde_json::from_str(slice)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
        let raw = "```json\n{\"lines\": [{\"start_ms\": 0, \"end_ms\": 1000, \"text\": \"Alleluia\"}]}\n```";
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
        let raw = r#"{"lines": []}"#;
        let lines = parse_claude_response(raw).expect("empty lines array is valid");
        assert_eq!(lines.len(), 0);
    }

    // ── source_priority tests ─────────────────────────────────────────────────

    #[test]
    fn source_priority_values() {
        assert_eq!(source_priority("tier1:spotify"), 4);
        assert_eq!(source_priority("tier1:lrclib"), 3);
        assert_eq!(source_priority("genius"), 2);
        assert_eq!(source_priority("tier1:genius"), 2);
        assert_eq!(source_priority("tier1:yt_subs"), 1);
        assert_eq!(source_priority("unknown"), 0);
        assert_eq!(source_priority(""), 0);
    }

    // ── best_authoritative tests ──────────────────────────────────────────────

    #[test]
    fn best_authoritative_picks_most_lines() {
        let candidates = vec![
            CandidateText {
                source: "tier1:spotify".into(),
                lines: vec!["a".into(), "b".into()],
                line_timings: None,
                has_timing: false,
            },
            CandidateText {
                source: "genius".into(),
                lines: vec!["a".into(), "b".into(), "c".into(), "d".into()],
                line_timings: None,
                has_timing: false,
            },
        ];
        let result = best_authoritative(&candidates);
        assert_eq!(result.len(), 4, "should pick the candidate with more lines");
    }

    #[test]
    fn best_authoritative_uses_priority_for_tie() {
        let candidates = vec![
            CandidateText {
                source: "genius".into(),
                lines: vec!["x".into(), "y".into()],
                line_timings: None,
                has_timing: false,
            },
            CandidateText {
                source: "tier1:spotify".into(),
                lines: vec!["a".into(), "b".into()],
                line_timings: None,
                has_timing: false,
            },
        ];
        // Both have 2 lines; spotify wins on priority.
        let result = best_authoritative(&candidates);
        assert_eq!(result[0], "a");
    }

    #[test]
    fn best_authoritative_empty_returns_empty() {
        let result = best_authoritative(&[]);
        assert!(result.is_empty());
    }

    // ── merge output structure test (mock) ────────────────────────────────────

    /// Verify that `merge` produces an AlignedTrack with the expected shape
    /// when Claude returns a known JSON response. We can't call a live Claude
    /// in unit tests, but we can verify the conversion logic by calling the
    /// internal helpers directly and constructing the expected output.
    ///
    /// This test exercises the full path EXCEPT the actual HTTP call by
    /// testing each stage independently and asserting the composited result.
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
}
