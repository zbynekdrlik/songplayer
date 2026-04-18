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
           one per reference word. A 'reference word' is a whitespace-separated \
           token in the reference text — NOT a linguistic word. Contractions \
           (couldn't, I'm, don't), possessives (God's), and hyphenated compounds \
           count as ONE reference word each. Do NOT split them. Do NOT merge \
           them. The count MUST equal {total_word_count} exactly, or the merge \
           will be rejected and the song loses its lyrics.\n\
        2. Provider word lists may tokenize differently from the reference \
           (provider ASR might output 'could nt' while reference has 'couldn't'). \
           Align them intelligently but ALWAYS emit one output entry per \
           REFERENCE token, never per provider token.\n\
        3. Providers have DIFFERENT RELIABILITY — see each provider's \
           base_confidence. Weight timings by base_confidence^2, NOT equal average. \
           Example: qwen3 at 0.7 and autosub at 0.3 → qwen3 gets weight 0.49, autosub \
           gets weight 0.09 (about 5:1 in favor of qwen3).\n\
        4. If providers disagree by >1000ms at a word, DO NOT average — take the timing \
           from the higher-base_confidence provider and emit c = that provider's base * 0.9 \
           (the disagreement itself is signal that something is noisy).\n\
        5. If providers agree (within 500ms), emit the weighted average from rule 3 and \
           set c = min(1.0, max(base_conf of participating providers) * 1.2) — agreement \
           across providers IS positive signal.\n\
        6. Single provider matched: use its timing with c = base_confidence * 0.7.\n\
        7. No provider matched: s=0, e=0, c=0.\n\
        8. Reject outliers >2000ms from median of participating providers.\n\
        9. Return ONLY the JSON object. No explanation. No preamble. No \
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
        // Previously this was a silent bail that propagated to a generic
        // "LLM merge failed" at the orchestrator level, hiding the exact
        // counts + response preview that an operator needs to diagnose
        // whether Claude hallucinated extra words, the reference text was
        // reshaped upstream, or the prompt's word-count rule wasn't
        // followed. Log before bailing so the next failure is actionable.
        warn!(
            "merge: word count mismatch — Claude returned {} timings, reference has {} words. \
             First 300 chars of cleaned response: {}",
            response.words.len(),
            total_ref_words,
            &cleaned[..cleaned.len().min(300)]
        );
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

    // Diagnostic: if merge confidence is worse than what a single best provider
    // would have given on the pass-through path (base_confidence * 0.7), log a
    // warning so operators can spot songs where the ensemble hurt rather than helped.
    let avg_merged_confidence: f32 = mean_confidence(
        &details
            .iter()
            .map(|d| d.merged_confidence)
            .collect::<Vec<_>>(),
    );
    let best_single_pass_through: f32 = provider_results
        .iter()
        .map(|pr| {
            pass_through_baseline(
                pr.metadata
                    .get("base_confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.7) as f32,
            )
        })
        .fold(0.0_f32, f32::max);
    if merge_regressed(avg_merged_confidence, best_single_pass_through) {
        warn!(
            avg_merged_confidence,
            best_single_pass_through,
            provider_count = provider_results.len(),
            "merge: ensemble confidence lower than best single-provider pass-through — \
             providers may have disagreed noisily; consider raising density gate"
        );
    }

    Ok((track, details))
}

/// Mean of a slice of confidences. Returns 0.0 for empty input.
pub(crate) fn mean_confidence(values: &[f32]) -> f32 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f32>() / values.len() as f32
    }
}

/// Pass-through baseline: what a single provider's pass-through would have
/// produced. Used in the diagnostic warn comparison.
pub(crate) fn pass_through_baseline(base_confidence: f32) -> f32 {
    base_confidence * 0.7
}

/// Returns true when the ensemble's merged average is strictly below the
/// best single-provider pass-through (i.e. the ensemble hurt rather than
/// helped). Used to gate the diagnostic warn log.
pub(crate) fn merge_regressed(avg_merged: f32, best_single: f32) -> bool {
    avg_merged < best_single
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
    fn build_merge_prompt_system_emphasizes_confidence_weighting() {
        // Prevents regression on the rule that Claude must weight by base_confidence
        // squared — not blindly average the providers. This is what caused the
        // h-A1Tzkjsi4 regression where autosub-heavy averaging dragged a song's
        // conf below single-provider baseline.
        let results = vec![
            ProviderResult {
                provider_name: "qwen3".into(),
                lines: vec![],
                metadata: serde_json::json!({"base_confidence": 0.7}),
            },
            ProviderResult {
                provider_name: "autosub".into(),
                lines: vec![],
                metadata: serde_json::json!({"base_confidence": 0.3}),
            },
        ];
        let (system, _) = build_merge_prompt("word one two", "m", &results);
        assert!(
            system.contains("base_confidence^2"),
            "system prompt must tell Claude to weight by base_confidence squared"
        );
        assert!(
            system.contains("disagree by >1000ms"),
            "system prompt must instruct high-confidence preference on disagreement"
        );
        assert!(
            system.contains("5:1"),
            "system prompt must give a concrete example ratio"
        );
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

    /// Regression: build_merge_prompt's line counting filter must skip empty
    /// lines. Deleting the `!` in `!l.trim().is_empty()` would count empty
    /// lines in line_count — this test catches it because the user prompt
    /// interpolates line_count as "N lines", so a wrong count is observable.
    #[test]
    fn build_merge_prompt_skips_empty_lines_in_line_count() {
        // "line one\n\nline two\n" has 2 non-empty lines.
        // Without the `!`, empty lines are counted → line_count = 3.
        let reference = "line one\n\nline two\n";
        let (_, user) = build_merge_prompt(reference, "yt_subs", &[]);
        // The user prompt contains "{line_count} lines", so assert exactly "2 lines".
        assert!(
            user.contains("2 lines"),
            "expected '2 lines' (blank line excluded), got user prompt: {}",
            &user[..200.min(user.len())]
        );
        assert!(
            !user.contains("3 lines"),
            "must not count the empty line, but found '3 lines' in user prompt"
        );
    }

    /// Regression: mean_confidence computes sum/len. Mutations tried % and *.
    #[test]
    fn mean_confidence_computes_correct_mean() {
        let result = mean_confidence(&[0.4, 0.8]);
        assert!(
            (result - 0.6).abs() < 1e-5,
            "mean([0.4, 0.8]) should be 0.6, got {result}"
        );
        assert_eq!(mean_confidence(&[]), 0.0, "empty slice must return 0.0");
        let single = mean_confidence(&[0.5]);
        assert!(
            (single - 0.5).abs() < 1e-5,
            "mean([0.5]) should be 0.5, got {single}"
        );
    }

    /// Regression: pass_through_baseline must multiply by 0.7. Mutations tried + and /.
    #[test]
    fn pass_through_baseline_multiplies_by_0_7() {
        let val = pass_through_baseline(1.0);
        assert!(
            (val - 0.7).abs() < 1e-5,
            "1.0 base → 0.7 baseline, got {val}"
        );
        let zero = pass_through_baseline(0.0);
        assert!(zero.abs() < 1e-5, "0.0 base → 0.0 baseline, got {zero}");
        let half = pass_through_baseline(0.5);
        assert!(
            (half - 0.35).abs() < 1e-5,
            "0.5 base → 0.35 baseline, got {half}"
        );
    }

    /// Regression: merge_regressed must be strict less-than. Mutations tried ==, >, <=.
    #[test]
    fn merge_regressed_strict_less_than() {
        assert!(merge_regressed(0.3, 0.5), "0.3 < 0.5 must return true");
        assert!(
            !merge_regressed(0.5, 0.5),
            "equal values must NOT trigger (strict <)"
        );
        assert!(!merge_regressed(0.7, 0.5), "higher avg must NOT trigger");
    }

    #[test]
    fn merge_prompt_pins_reference_tokenization_on_contractions() {
        // Regression: "Have To Have You" failed merge on 2026-04-19 because
        // Claude tokenized contractions like "couldn't" as two words while
        // our `split_whitespace().count()` counts it as one, producing a
        // word-count mismatch that bailed the merge and cost the song its
        // lyrics. The prompt MUST tell Claude that contractions are ONE
        // reference word.
        let providers: Vec<ProviderResult> = vec![];
        let (system, user) = build_merge_prompt("Hello world", "yt_subs", &providers);
        // Must mention contractions explicitly and specify they are single tokens.
        assert!(
            system.contains("contraction") || system.contains("Contraction"),
            "system prompt must cover contraction tokenization: {system}"
        );
        assert!(
            system.contains("whitespace"),
            "system prompt must anchor on whitespace tokenization: {system}"
        );
        assert!(
            system.contains("ONE") || system.contains("one"),
            "system prompt must state contractions count as ONE reference word: {system}"
        );
        // Unused in this assertion but kept for sanity.
        let _ = user;
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
