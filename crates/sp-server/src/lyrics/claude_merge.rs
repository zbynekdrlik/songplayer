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

/// Merge WhisperX timing with authoritative reference text.
///
/// Returns an `AlignedTrack` with line-level timing only (`words: None` on
/// every line — per `feedback_line_timing_only.md`).
///
/// Two paths based on the best authoritative candidate's source label:
///
/// 1. `description` / `override` — clean text from a trusted human / extractor
///    source. Goes through the deterministic mapper which preserves the
///    reference's exact line count by construction (no Claude call, no
///    re-segmentation by audio phrasing). Provenance: `"{source}+{asr.provenance}"`.
///    Fixes issue #78 where description's natural 25-line output was being
///    re-segmented into 95 audio-phrase fragments unusable on the LED wall.
///
/// 2. `genius` / other text-only — Claude semantic merge corrects WhisperX
///    mishearings against the reference. Provenance: `"{asr.provenance}+claude-merge"`.
pub async fn merge(
    ai_client: &AiClient,
    asr: &AlignedTrack,
    text_candidates: &[CandidateText],
) -> Result<AlignedTrack, MergeError> {
    // Step 1: pick authoritative reference candidate (whole CandidateText, not
    // just its lines — we need the source label to choose the merge path).
    let best = match text_candidates
        .iter()
        .max_by_key(|c| (source_priority(&c.source), c.lines.len()))
    {
        Some(b) if !b.lines.is_empty() => b,
        _ => return Err(MergeError::NoReference),
    };

    // Step 1a: description / override sources go through the deterministic
    // mapper. No Claude round-trip. Output line count == reference line count
    // by construction. Per `feedback_no_even_distribution.md`, timing comes
    // from word-level LCS alignment between reference text and ASR words —
    // never uniform/even-distribution.
    if best.source == "description" || best.source == "override" {
        return Ok(merge_deterministic(asr, &best.lines, &best.source));
    }

    let reference_lines = best.lines.clone();

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
            tracing::warn!(
                raw_len = raw.len(),
                first_200 = %raw.chars().take(200).collect::<String>(),
                "claude_merge: parse failed"
            );
            return Err(e);
        }
    };

    if merged_lines.is_empty() {
        return Err(MergeError::ParseFailed(
            "zero lines (refusal or empty); fall back to raw WhisperX".into(),
        ));
    }

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

/// Tie-break order for `best_authoritative` (production labels match gather.rs).
fn source_priority(source: &str) -> u32 {
    if source == "override" {
        5
    } else if source.starts_with("tier1:spotify") {
        4
    } else if source == "lrclib" || source.starts_with("tier1:lrclib") {
        3
    } else if source == "genius" || source.starts_with("tier1:genius") {
        2
    } else if source == "yt_subs" || source.starts_with("tier1:yt_subs") {
        1
    } else {
        0
    }
}

/// Pick the strongest authoritative source: source priority wins; ties broken by
/// line count (longest wins). Per #72: high-priority short candidates beat
/// longer noisy low-priority ones.
fn best_authoritative(candidates: &[CandidateText]) -> Vec<String> {
    candidates
        .iter()
        .max_by_key(|c| (source_priority(&c.source), c.lines.len()))
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

// ── Deterministic mapper for description / override sources ───────────────────
//
// Why this exists: when the authoritative reference text comes from a clean,
// segmented source (YouTube-description extraction or operator override),
// the reference's line breaks are themselves the contract we must ship. The
// pre-fix Claude merge path iterated WhisperX *audio phrases* (split on
// >500ms gaps) and emitted one output line per phrase, so a 6-min worship
// song with 60 phrases produced 60+ output lines — destroying the
// reference's 25-line natural segmentation. Issue #78.
//
// The mapper aligns reference words to ASR words via LCS, then for each
// reference line collects the matched ASR words' min/max timing. Output line
// count == reference line count by construction. No LLM round-trip.

/// Normalize a word for cross-source matching: lowercase, alphanumeric only.
/// Strips punctuation so "He's" and "hes" match; "praise," and "praise" match.
fn normalize_word_for_match(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// LCS-based word alignment. Returns `alignment[i] = Some(j)` if
/// `ref_words[i]` is matched to `asr_words[j]`, else `None`.
///
/// Standard dynamic-programming LCS over the two normalized word sequences.
/// Tolerates ASR mishearings (unmatched ASR words skipped) and reference
/// ad-libs absent from audio (unmatched reference words flagged None).
fn lcs_align(ref_words: &[String], asr_words: &[String]) -> Vec<Option<usize>> {
    let n = ref_words.len();
    let m = asr_words.len();
    if n == 0 || m == 0 {
        return vec![None; n];
    }

    // dp[i][j] = LCS length of ref_words[..i] and asr_words[..j].
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if ref_words[i] == asr_words[j] {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // Backtrack to recover the alignment. Walk from (n,m) back; whenever
    // ref_words[i-1] == asr_words[j-1] we record a match.
    let mut alignment = vec![None; n];
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if ref_words[i - 1] == asr_words[j - 1] {
            alignment[i - 1] = Some(j - 1);
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    alignment
}

/// Map clean reference lines onto ASR word timings deterministically.
///
/// Output: an `AlignedTrack` with exactly `reference_lines.len()` lines, each
/// carrying the reference line's text verbatim and a `[start_ms, end_ms]`
/// window derived from the LCS-matched ASR words. Strictly chronological
/// (`line[i].end_ms <= line[i+1].start_ms`) by floor-clamping.
///
/// `best_source` is the authoritative source label (`"description"` or
/// `"override"` in production). Provenance is `"{best_source}+{asr.provenance}"`.
pub(super) fn merge_deterministic(
    asr: &AlignedTrack,
    reference_lines: &[String],
    best_source: &str,
) -> AlignedTrack {
    // Tokenize the reference into (normalized_word, ref_line_idx) pairs.
    let mut ref_pairs: Vec<(String, usize)> = Vec::new();
    for (idx, line) in reference_lines.iter().enumerate() {
        for w in line.split_whitespace() {
            let n = normalize_word_for_match(w);
            if !n.is_empty() {
                ref_pairs.push((n, idx));
            }
        }
    }

    // Flatten ASR per-word timings (drop hallucinated lead-ins consistent with
    // the Claude path so both branches see the same audio cleaning).
    let mut asr_words: Vec<(String, u32, u32)> = Vec::new();
    for line in &asr.lines {
        let words = match &line.words {
            Some(w) if !w.is_empty() => w.clone(),
            _ => continue,
        };
        let words = drop_hallucinated_lead_in(words);
        for w in &words {
            let n = normalize_word_for_match(&w.text);
            if !n.is_empty() {
                asr_words.push((n, w.start_ms, w.end_ms));
            }
        }
    }

    // LCS alignment of normalized reference words to normalized ASR words.
    let ref_only: Vec<String> = ref_pairs.iter().map(|(w, _)| w.clone()).collect();
    let asr_only: Vec<String> = asr_words.iter().map(|(w, _, _)| w.clone()).collect();
    let alignment = lcs_align(&ref_only, &asr_only);

    // For each reference line, gather its matched ASR word indices.
    let n_lines = reference_lines.len();
    let mut line_asr_indices: Vec<Vec<usize>> = vec![Vec::new(); n_lines];
    for (ref_idx, (_w, line_idx)) in ref_pairs.iter().enumerate() {
        if let Some(asr_idx) = alignment[ref_idx] {
            line_asr_indices[*line_idx].push(asr_idx);
        }
    }

    // Compute per-line timing from matched indices (min start, max end).
    // Unmatched lines get None and are filled in a second pass.
    let mut starts: Vec<Option<u32>> = vec![None; n_lines];
    let mut ends: Vec<Option<u32>> = vec![None; n_lines];
    for (line_idx, indices) in line_asr_indices.iter().enumerate() {
        if let Some(&min_i) = indices.iter().min() {
            starts[line_idx] = Some(asr_words[min_i].1);
        }
        if let Some(&max_i) = indices.iter().max() {
            ends[line_idx] = Some(asr_words[max_i].2);
        }
    }

    // Second pass: emit AlignedLine per reference line with strictly
    // increasing timings. For unmatched lines we use a small placeholder
    // (200ms) starting at the previous line's end — this is NOT a uniform
    // distribution (per `feedback_no_even_distribution.md`); it's a minimal
    // window for a line the audio doesn't sing, so the wall briefly displays
    // the reference text without breaking timeline monotonicity.
    let mut output_lines: Vec<AlignedLine> = Vec::with_capacity(n_lines);
    let mut floor: u32 = 0;
    for (idx, text) in reference_lines.iter().enumerate() {
        let raw_start = starts[idx].unwrap_or(floor);
        let raw_end = ends[idx].unwrap_or(raw_start.saturating_add(200));
        let s = raw_start.max(floor);
        let e = raw_end.max(s.saturating_add(1));
        output_lines.push(AlignedLine {
            text: text.clone(),
            start_ms: s,
            end_ms: e,
            words: None,
        });
        floor = e;
    }

    AlignedTrack {
        lines: output_lines,
        provenance: format!("{}+{}", best_source, asr.provenance),
        raw_confidence: asr.raw_confidence,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "claude_merge_tests.rs"]
mod tests;
