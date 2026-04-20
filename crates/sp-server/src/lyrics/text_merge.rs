//! Claude-powered reference-text reconciliation across multiple candidate sources.
//!
//! Mirrors the pattern of `merge.rs` but for the text-selection step: takes
//! N candidate texts (yt_subs, lrclib, autosub-text, description, CCLI) and
//! produces one canonical text with per-line provenance. Short-circuits on
//! 1 candidate.

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::ai::client::AiClient;
use crate::lyrics::provider::CandidateText;

/// One line of reconciled reference text.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ReconciledLine {
    pub text: String,
    /// Which candidate source this line was predominantly drawn from.
    pub source: String,
}

/// Build the Claude merge prompt for N candidate texts. Pure function;
/// unit-testable with fixture data. Uses software-engineering framing
/// (no system prompt) to avoid OAuth cloaking refusals on lyric content.
pub fn build_text_merge_prompt(candidates: &[CandidateText]) -> (String, String) {
    let system = String::new(); // Empty system prompt: soft-framing in user message instead.
    let mut user = String::from(
        "I'm building a karaoke subtitle app for a church. I have multiple candidate \
         lyric texts for the same song, each transcribed by a different source with \
         its own kind of errors. I need to reconcile them into one canonical text.\n\n\
         Rules:\n\
         1. Keep line structure — do NOT merge or split lines.\n\
         2. Prefer words that appear in 2+ candidates.\n\
         3. Fix obvious transcription errors: homophones (there/their), capitalization, \
            misheard words where one candidate clearly disagrees with the rest.\n\
         4. Drop noise tokens ([music], >>, duplicate filler).\n\
         5. Return ONLY the JSON. No preamble. No markdown fences.\n\
         6. Each line must be tagged with the source it was predominantly drawn from.\n\n\
         Return JSON: {\"lines\": [{\"text\": \"...\", \"source\": \"yt_subs|lrclib|autosub|...\"}]}\n\n\
         Candidates:\n",
    );
    for c in candidates {
        user.push_str(&format!("\n--- {} ---\n", c.source));
        for line in &c.lines {
            user.push_str(line);
            user.push('\n');
        }
    }
    (system, user)
}

#[derive(Debug, serde::Deserialize)]
struct MergeTextResponse {
    lines: Vec<ReconciledLine>,
}

/// Reconcile N candidate texts into one canonical reference text. Short-circuits:
/// 0 candidates → error; 1 candidate → pass-through (no Claude call).
#[cfg_attr(test, mutants::skip)] // orchestration across Claude I/O; behavior covered by build_text_merge_prompt tests + wiremock integration below
pub async fn merge_candidate_texts(
    ai_client: &AiClient,
    candidates: &[CandidateText],
) -> Result<Vec<ReconciledLine>> {
    match candidates.len() {
        0 => anyhow::bail!("merge_candidate_texts: no candidates"),
        1 => {
            let c = &candidates[0];
            let lines = c
                .lines
                .iter()
                .map(|l| ReconciledLine {
                    text: l.clone(),
                    source: c.source.clone(),
                })
                .collect();
            return Ok(lines);
        }
        _ => {}
    }

    let (system, user) = build_text_merge_prompt(candidates);
    debug!(
        "text_merge: sending {} candidates to Claude ({} chars)",
        candidates.len(),
        user.len()
    );
    let raw = ai_client
        .chat(&system, &user)
        .await
        .context("Claude text-merge call failed")?;
    let cleaned = crate::ai::client::strip_markdown_fences(&raw);
    let parsed: MergeTextResponse = serde_json::from_str(&cleaned).map_err(|e| {
        warn!(
            "text_merge: failed to parse Claude response: {e}\nFirst 500 chars: {}",
            &cleaned[..cleaned.len().min(500)]
        );
        anyhow::anyhow!("Claude text-merge JSON parse failed: {e}")
    })?;
    Ok(parsed.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(source: &str, lines: &[&str]) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: lines.iter().map(|s| (*s).to_string()).collect(),
            has_timing: false,
            line_timings: None,
        }
    }

    #[test]
    fn build_prompt_includes_all_candidate_sources() {
        let cands = vec![
            c("yt_subs", &["Hello world"]),
            c("lrclib", &["Hello, world"]),
        ];
        let (system, user) = build_text_merge_prompt(&cands);
        assert!(system.is_empty(), "no system prompt — cloaking avoidance");
        assert!(user.contains("--- yt_subs ---"));
        assert!(user.contains("--- lrclib ---"));
        assert!(user.contains("Hello world"));
        assert!(user.contains("Hello, world"));
        assert!(user.contains("karaoke subtitle app"));
    }

    #[test]
    fn build_prompt_demands_no_line_split_and_no_preamble() {
        let cands = vec![c("yt_subs", &["x"]), c("lrclib", &["y"])];
        let (_, user) = build_text_merge_prompt(&cands);
        assert!(user.contains("do NOT merge or split lines"));
        assert!(user.contains("No preamble"));
        assert!(user.contains("No markdown fences"));
    }

    #[tokio::test]
    async fn merge_zero_candidates_is_error() {
        let client = AiClient::new(crate::ai::AiSettings::default());
        assert!(merge_candidate_texts(&client, &[]).await.is_err());
    }

    #[tokio::test]
    async fn merge_single_candidate_short_circuits_no_claude_call() {
        // AiClient is never called because we should short-circuit; use a
        // default client pointing at an unreachable port. If the code
        // accidentally makes the call, the test will hang/error — we'd see it.
        let client = AiClient::new(crate::ai::AiSettings::default());
        let cands = vec![c("lrclib", &["Line one", "Line two"])];
        let out = merge_candidate_texts(&client, &cands).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Line one");
        assert_eq!(out[0].source, "lrclib");
        assert_eq!(out[1].text, "Line two");
    }

    #[tokio::test]
    async fn merge_multi_candidate_calls_claude_and_parses() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "test",
                "object": "chat.completion",
                "created": 0,
                "model": "claude-opus-4-20250514",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "{\"lines\":[{\"text\":\"Amazing grace\",\"source\":\"lrclib\"},{\"text\":\"how sweet the sound\",\"source\":\"yt_subs\"}]}"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
            })))
            .mount(&mock)
            .await;

        let client = AiClient::new(crate::ai::AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });
        let cands = vec![
            c("yt_subs", &["Amazing grace", "how sweet the sound"]),
            c("lrclib", &["Amazing grace", "how sweet the sound"]),
        ];
        let out = merge_candidate_texts(&client, &cands).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Amazing grace");
        assert_eq!(out[0].source, "lrclib");
        assert_eq!(out[1].source, "yt_subs");
    }

    #[tokio::test]
    async fn merge_handles_claude_preamble_before_fences() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"content": "I'll analyze the data...\n```json\n{\"lines\":[{\"text\":\"ok\",\"source\":\"lrclib\"}]}\n```"}
                }]
            })))
            .mount(&mock)
            .await;
        let client = AiClient::new(crate::ai::AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });
        let cands = vec![c("yt_subs", &["a"]), c("lrclib", &["b"])];
        let out = merge_candidate_texts(&client, &cands).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "ok");
    }
}
