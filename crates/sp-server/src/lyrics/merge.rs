//! LLM-powered merge layer for ensemble alignment.
//!
//! Accepts 1–N `ProviderResult`s, constructs a Claude Opus prompt,
//! parses the JSON response into a merged `LyricsTrack`, and writes
//! an audit log.

use anyhow::{Context, Result};
use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};
use std::path::Path;
use tokio::fs;
use tracing::{debug, warn};

use crate::ai::client::AiClient;
use crate::lyrics::provider::*;

/// Build the merge prompt for Claude Opus.
///
/// Pure function (no I/O) — unit-testable with fixture data.
pub fn build_merge_prompt(
    reference_text: &str,
    reference_source: &str,
    provider_results: &[ProviderResult],
) -> (String, String) {
    let system = "You are a lyrics alignment merger. You receive word-level \
        timestamp results from multiple independent alignment providers for \
        the same song. Produce a single merged result with the best possible \
        timing for each word.\n\n\
        CRITICAL RULES:\n\
        1. You MUST return ALL lines from the reference text. Every single \
           line. Do not skip, summarize, or show examples.\n\
        2. Match provider words to reference text intelligently \
           (contractions, ASR errors, abbreviations).\n\
        3. Multiple providers matched: weighted average of timings. Reject \
           outliers >2000ms from median.\n\
        4. Single provider matched: use its timing with confidence scaled \
           to base_confidence * 0.7.\n\
        5. No provider matched: zero-timed placeholder, confidence 0.\n\
        6. Gap >2000ms between adjacent words within a line: set \
           display_split=true on that line.\n\
        7. Return ONLY the JSON object. No explanation. No preamble. No \
           markdown fences. Start your response with { and end with }."
        .to_string();

    // Count reference lines for the explicit instruction
    let line_count = reference_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    let mut user = String::new();
    user.push_str(&format!(
        "Reference text (source: {reference_source}, {line_count} lines):\n\
         {reference_text}\n\n"
    ));
    user.push_str("Provider results:\n");
    for pr in provider_results {
        user.push_str(&format!(
            "\n--- {} (base_confidence: {}) ---\n",
            pr.provider_name,
            pr.metadata
                .get("base_confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.7)
        ));
        for line in &pr.lines {
            let words_str: Vec<String> = line
                .words
                .iter()
                .map(|w| format!("{}@{}ms", w.text, w.start_ms))
                .collect();
            user.push_str(&format!("  [{}] {}\n", line.start_ms, words_str.join(" ")));
        }
    }
    user.push_str(&format!(
        "\nReturn JSON with EXACTLY {line_count} lines (one per reference line):\n\
         {{\"lines\": [{{\"text\": \"full line\", \"start_ms\": N, \"end_ms\": N, \
         \"display_split\": false, \"words\": [{{\"text\": \"word\", \"start_ms\": N, \
         \"end_ms\": N, \"confidence\": 0.63, \"sources_agreed\": 1, \
         \"spread_ms\": 0}}]}}]}}"
    ));

    (system, user)
}

/// Parsed LLM merge response.
#[derive(Debug, serde::Deserialize)]
struct MergeResponse {
    lines: Vec<MergeResponseLine>,
}

#[derive(Debug, serde::Deserialize)]
struct MergeResponseLine {
    text: String,
    start_ms: u64,
    end_ms: u64,
    /// Used by callers to decide if a visual gap should be inserted before this line.
    #[serde(default)]
    #[allow(dead_code)]
    display_split: bool,
    words: Vec<MergeResponseWord>,
}

#[derive(Debug, serde::Deserialize)]
struct MergeResponseWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
    confidence: f32,
    /// Number of providers that agreed on this word's timing.
    #[serde(default)]
    #[allow(dead_code)]
    sources_agreed: u8,
    #[serde(default)]
    spread_ms: u32,
}

/// Run the LLM merge: send prompt to Claude, parse response, return
/// merged LyricsTrack + audit data.
#[cfg_attr(test, mutants::skip)]
pub async fn merge_provider_results(
    ai_client: &AiClient,
    reference_text: &str,
    reference_source: &str,
    provider_results: &[ProviderResult],
) -> Result<(LyricsTrack, Vec<WordMergeDetail>)> {
    let (system, user) = build_merge_prompt(reference_text, reference_source, provider_results);

    debug!(
        "merge: sending {} providers to Claude ({} chars prompt)",
        provider_results.len(),
        user.len()
    );

    let raw_response = ai_client
        .chat_with_timeout(&system, &user, 600)
        .await
        .context("LLM merge HTTP call failed")?;

    debug!("merge: Claude returned {} chars", raw_response.len());

    // Strip markdown fences and parse
    let cleaned = crate::ai::client::strip_markdown_fences(&raw_response);
    let response: MergeResponse = serde_json::from_str(&cleaned).map_err(|e| {
        warn!(
            "merge: failed to parse Claude response as JSON: {e}\nFirst 500 chars: {}",
            &cleaned[..cleaned.len().min(500)]
        );
        anyhow::anyhow!("LLM merge JSON parse failed: {e}")
    })?;

    // Convert MergeResponse → LyricsTrack
    let lines: Vec<LyricsLine> = response
        .lines
        .iter()
        .map(|l| LyricsLine {
            start_ms: l.start_ms,
            end_ms: l.end_ms,
            en: l.text.clone(),
            sk: None,
            words: Some(
                l.words
                    .iter()
                    .map(|w| LyricsWord {
                        text: w.text.clone(),
                        start_ms: w.start_ms,
                        end_ms: w.end_ms,
                    })
                    .collect(),
            ),
        })
        .collect();

    let track = LyricsTrack {
        version: 2,
        source: format!(
            "ensemble:{}",
            provider_results
                .iter()
                .map(|p| p.provider_name.as_str())
                .collect::<Vec<_>>()
                .join("+")
        ),
        language_source: "en".into(),
        language_translation: String::new(),
        lines,
    };

    // Build audit details from response
    let mut details = Vec::new();
    let mut word_idx = 0;
    for line in &response.lines {
        for word in &line.words {
            details.push(WordMergeDetail {
                word_index: word_idx,
                reference_text: word.text.clone(),
                provider_estimates: Vec::new(),
                outliers_rejected: Vec::new(),
                merged_start_ms: word.start_ms,
                merged_confidence: word.confidence,
                spread_ms: word.spread_ms,
            });
            word_idx += 1;
        }
    }

    Ok((track, details))
}

/// Write the audit log to disk alongside the lyrics JSON.
#[cfg_attr(test, mutants::skip)]
pub async fn write_audit_log(cache_dir: &Path, log: &AuditLog) -> Result<()> {
    let path = cache_dir.join(format!("{}_alignment_audit.json", log.video_id));
    let json = serde_json::to_string_pretty(log)?;
    fs::write(&path, json).await?;
    debug!("wrote audit log to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_merge_prompt_includes_reference_and_providers() {
        let results = vec![ProviderResult {
            provider_name: "qwen3".into(),
            lines: vec![LineTiming {
                text: "Hello world".into(),
                start_ms: 1000,
                end_ms: 2000,
                words: vec![
                    WordTiming {
                        text: "Hello".into(),
                        start_ms: 1000,
                        end_ms: 1500,
                        confidence: 0.9,
                    },
                    WordTiming {
                        text: "world".into(),
                        start_ms: 1500,
                        end_ms: 2000,
                        confidence: 0.9,
                    },
                ],
            }],
            metadata: serde_json::json!({}),
        }];
        let (system, user) = build_merge_prompt("Hello world", "manual_subs", &results);
        assert!(system.contains("lyrics alignment merger"));
        assert!(user.contains("Hello world"));
        assert!(user.contains("manual_subs"));
        assert!(user.contains("qwen3"));
        assert!(user.contains("Hello@1000ms"));
    }

    #[test]
    fn build_merge_prompt_handles_multiple_providers() {
        let results = vec![
            ProviderResult {
                provider_name: "qwen3".into(),
                lines: vec![],
                metadata: serde_json::json!({}),
            },
            ProviderResult {
                provider_name: "autosub".into(),
                lines: vec![],
                metadata: serde_json::json!({}),
            },
        ];
        let (_, user) = build_merge_prompt("test", "lrclib", &results);
        assert!(user.contains("qwen3"));
        assert!(user.contains("autosub"));
    }

    #[test]
    fn build_merge_prompt_line_count_excludes_empty_lines() {
        // Kills the `!l.trim().is_empty()` mutation: if ! is removed,
        // the filter counts ONLY empty lines. Reference text has 3 non-empty
        // lines and 2 empty lines; prompt must say "3 lines", not "2".
        let ref_text = "line one\n\nline two\n   \nline three";
        let (_, user) = build_merge_prompt(ref_text, "manual_subs", &[]);
        assert!(
            user.contains("3 lines"),
            "expected 3 lines in prompt, got: {user}"
        );
        assert!(
            user.contains("EXACTLY 3 lines"),
            "expected explicit count in output instruction"
        );
        assert!(
            !user.contains("0 lines") && !user.contains("2 lines"),
            "line count must exclude empty/whitespace lines"
        );
    }

    #[test]
    fn parse_merge_response_json() {
        let json = r#"{
            "lines": [{
                "text": "Hello world",
                "start_ms": 1000,
                "end_ms": 2000,
                "display_split": false,
                "words": [
                    {"text": "Hello", "start_ms": 1000, "end_ms": 1500, "confidence": 0.95, "sources_agreed": 2, "spread_ms": 50},
                    {"text": "world", "start_ms": 1500, "end_ms": 2000, "confidence": 0.9, "sources_agreed": 1, "spread_ms": 0}
                ]
            }]
        }"#;
        let parsed: MergeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].words.len(), 2);
        assert_eq!(parsed.lines[0].words[0].confidence, 0.95);
    }
}
