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
    // Count reference lines and total words for the explicit instructions
    let line_count = reference_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    let total_word_count: usize = reference_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().count())
        .sum();

    let system = format!(
        "You are a lyrics alignment merger. You receive word-level \
        timestamp results from multiple independent alignment providers for \
        the same song. Produce a single merged result with the best possible \
        timing for each word.\n\n\
        CRITICAL RULES:\n\
        1. You MUST return EXACTLY {total_word_count} word entries, in reading order, \
           one per reference word.\n\
        2. Match provider words to reference text intelligently \
           (contractions, ASR errors, abbreviations).\n\
        3. Multiple providers matched: weighted average of timings. Reject \
           outliers >2000ms from median.\n\
        4. Single provider matched: use its timing with confidence scaled \
           to base_confidence * 0.7.\n\
        5. No provider matched: zero-timed placeholder, confidence 0.\n\
        6. Return ONLY the JSON object. No explanation. No preamble. No \
           markdown fences. Start your response with {{ and end with }}."
    );

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
        "\nReturn JSON with EXACTLY {total_word_count} word timings in reading order \
         (no line structure, no text, no extra fields):\n\
         {{\"words\": [{{\"s\": N, \"e\": N, \"c\": 0.63}}, ...]}}\n\n\
         The reference text has {total_word_count} words across {line_count} lines. \
         Match provider words to reference words intelligently (contractions, ASR errors) \
         and emit one entry per reference word in the order they appear in the reference text. \
         If no provider matched a word, emit {{\"s\": 0, \"e\": 0, \"c\": 0}}."
    ));

    (system, user)
}

/// Parsed LLM merge response — compact per-word format.
#[derive(Debug, serde::Deserialize)]
struct MergeResponse {
    words: Vec<MergeResponseWord>,
}

#[derive(Debug, serde::Deserialize)]
struct MergeResponseWord {
    /// start_ms
    s: u64,
    /// end_ms
    e: u64,
    /// confidence (0.0–1.0)
    c: f32,
}

/// Run the LLM merge: send prompt to Claude, parse response, return
/// merged LyricsTrack + audit data.
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
        .chat(&system, &user)
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

    // Reconstruct LyricsTrack from the reference text structure + compact word timings.
    // The reference text carries line structure and word identity; response.words slots
    // timings in flat reading order.
    let ref_lines: Vec<&str> = reference_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    let ref_words_per_line: Vec<Vec<&str>> = ref_lines
        .iter()
        .map(|l| l.split_whitespace().collect())
        .collect();

    let total_ref_words: usize = ref_words_per_line.iter().map(|v| v.len()).sum();

    if response.words.len() != total_ref_words {
        anyhow::bail!(
            "Claude returned {} word timings but reference has {} words",
            response.words.len(),
            total_ref_words
        );
    }

    let mut cursor = 0;
    let mut lines: Vec<LyricsLine> = Vec::with_capacity(ref_lines.len());
    for (line_idx, ref_words) in ref_words_per_line.iter().enumerate() {
        let n = ref_words.len();
        let word_entries = &response.words[cursor..cursor + n];
        cursor += n;

        let words: Vec<LyricsWord> = ref_words
            .iter()
            .zip(word_entries.iter())
            .map(|(text, w)| LyricsWord {
                text: (*text).to_string(),
                start_ms: w.s,
                end_ms: w.e,
            })
            .collect();

        let line_start = words.first().map(|w| w.start_ms).unwrap_or(0);
        let line_end = words.last().map(|w| w.end_ms).unwrap_or(0);

        lines.push(LyricsLine {
            start_ms: line_start,
            end_ms: line_end,
            en: ref_lines[line_idx].to_string(),
            sk: None,
            words: Some(words),
        });
    }

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

    // Build audit details from the compact response
    let details: Vec<WordMergeDetail> = response
        .words
        .iter()
        .enumerate()
        .map(|(word_idx, w)| WordMergeDetail {
            word_index: word_idx,
            reference_text: String::new(),
            provider_estimates: Vec::new(),
            outliers_rejected: Vec::new(),
            merged_start_ms: w.s,
            merged_confidence: w.c,
            spread_ms: 0,
        })
        .collect();

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
        // Reference text has 3 non-empty lines × 2 words each = 6 words.
        // Prompt must say "6 word" and "EXACTLY 6", not line-based counts.
        let ref_text = "line one\n\nline two\n   \nline three";
        let (_, user) = build_merge_prompt(ref_text, "manual_subs", &[]);
        assert!(
            user.contains("6 word"),
            "expected 6 word count in prompt, got: {user}"
        );
        assert!(
            user.contains("EXACTLY 6"),
            "expected explicit word count in output instruction"
        );
        assert!(
            !user.contains("EXACTLY 3 lines") && !user.contains("EXACTLY 2"),
            "must not use old line-based count in output instruction"
        );
    }

    #[test]
    fn parse_merge_response_json() {
        let json = r#"{
            "words": [
                {"s": 1000, "e": 1500, "c": 0.95},
                {"s": 1500, "e": 2000, "c": 0.9}
            ]
        }"#;
        let parsed: MergeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.words.len(), 2);
        assert_eq!(parsed.words[0].c, 0.95);
    }

    #[tokio::test]
    async fn merge_reconstructs_lyricstrack_from_compact_claude_response() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": "{\"words\":[{\"s\":1000,\"e\":1500,\"c\":0.9},{\"s\":1500,\"e\":2000,\"c\":0.9},{\"s\":3000,\"e\":3500,\"c\":0.8}]}"
                        }
                    }]
                })),
            )
            .mount(&mock)
            .await;

        let client = AiClient::new(crate::ai::AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });

        let reference = "Hello world\nAgain";
        let providers = vec![ProviderResult {
            provider_name: "qwen3".into(),
            lines: vec![],
            metadata: serde_json::json!({}),
        }];

        let (track, details) =
            merge_provider_results(&client, reference, "manual_subs", &providers)
                .await
                .unwrap();
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "Hello world");
        assert_eq!(track.lines[0].words.as_ref().unwrap().len(), 2);
        assert_eq!(track.lines[0].words.as_ref().unwrap()[0].text, "Hello");
        assert_eq!(track.lines[0].words.as_ref().unwrap()[0].start_ms, 1000);
        assert_eq!(track.lines[1].en, "Again");
        assert_eq!(track.lines[1].words.as_ref().unwrap()[0].text, "Again");
        assert_eq!(track.lines[1].words.as_ref().unwrap()[0].start_ms, 3000);
        assert_eq!(details.len(), 3);
    }

    #[tokio::test]
    async fn merge_bails_when_word_count_mismatches_reference() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": "{\"words\":[{\"s\":1,\"e\":2,\"c\":0.5}]}"
                        }
                    }]
                })),
            )
            .mount(&mock)
            .await;
        let client = AiClient::new(crate::ai::AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "m".into(),
            system_prompt_extra: None,
        });
        let reference = "Hello world"; // 2 words; mock returns 1
        let providers = vec![ProviderResult {
            provider_name: "qwen3".into(),
            lines: vec![],
            metadata: serde_json::json!({}),
        }];
        let err = merge_provider_results(&client, reference, "m", &providers).await;
        assert!(err.is_err(), "must bail on word-count mismatch");
    }
}
