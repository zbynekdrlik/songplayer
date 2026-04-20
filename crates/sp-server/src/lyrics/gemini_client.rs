//! HTTP client for Gemini's `/v1beta/models/{model}:generateContent` endpoint.
//!
//! Supports two modes:
//! - Direct API: base_url = `https://generativelanguage.googleapis.com`, authenticated via
//!   `x-goog-api-key` header (the user's `gemini_api_key` from the SongPlayer DB).
//! - Proxy: base_url = local CLIProxy (`http://127.0.0.1:18787`), no auth header (proxy's
//!   OAuth is transparent). Kept as fallback; Phase 0 showed direct-API has the higher
//!   quota.
//!
//! `thinkingConfig.thinkingBudget = 2048` is set because Phase 0 validated that
//! unlimited thinking caused Gemini 3.x Pro to hallucinate duplicates + timeout
//! on dense worship audio.

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_THINKING_BUDGET: i32 = 2048;

pub struct GeminiClient {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub timeout_s: u64,
}

impl GeminiClient {
    /// Direct-API client. Calls `https://generativelanguage.googleapis.com` with
    /// an `x-goog-api-key` header.
    pub fn direct(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            model: model.into(),
            api_key: Some(api_key.into()),
            timeout_s: 300,
        }
    }

    /// Proxy client. Calls a local CLIProxy (no auth header).
    pub fn proxy(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            timeout_s: 300,
        }
    }

    /// Send prompt + audio to Gemini, return the text body from the first candidate.
    pub async fn transcribe_chunk(&self, prompt: &str, audio_wav: &Path) -> Result<String> {
        let bytes = tokio::fs::read(audio_wav)
            .await
            .with_context(|| format!("read chunk audio {audio_wav:?}"))?;
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let body = json!({
            "contents": [{
                "parts": [
                    {"text": prompt},
                    {"inline_data": {"mime_type": "audio/wav", "data": audio_b64}}
                ]
            }],
            "generationConfig": {
                "temperature": 0.0,
                "thinkingConfig": {"thinkingBudget": DEFAULT_THINKING_BUDGET}
            }
        });

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_s))
            .build()
            .context("build reqwest client")?;
        let mut req = client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.header("x-goog-api-key", key.as_str());
        }
        let resp = req.send().await.context("POST to Gemini")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            anyhow::bail!("gemini call failed: HTTP {status}: {text}");
        }
        let doc: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parse JSON: {text}"))?;
        let out = doc
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("no text in candidates[0]: {text}"))?;
        Ok(out.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_tmp_wav() -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &[0u8; 16]).unwrap();
        tmp
    }

    #[tokio::test]
    async fn transcribe_chunk_extracts_text_from_first_candidate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "(00:01.0 --> 00:02.0) hello"}
                        ]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let out = client.transcribe_chunk("prompt", tmp.path()).await.unwrap();
        assert_eq!(out, "(00:01.0 --> 00:02.0) hello");
    }

    #[tokio::test]
    async fn transcribe_chunk_errors_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("HTTP 500"), "err = {err}");
    }

    #[tokio::test]
    async fn transcribe_chunk_errors_when_no_candidates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidates": []})))
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(
            format!("{err}").contains("no text in candidates"),
            "err = {err}"
        );
    }

    #[tokio::test]
    async fn direct_api_client_sends_api_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .and(header("x-goog-api-key", "AIza-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        // Override base_url to point at our mock (direct() hardcodes the real Google URL).
        let client = GeminiClient {
            base_url: server.uri(),
            model: "gemini-3.1-pro-preview".into(),
            api_key: Some("AIza-test-key".into()),
            timeout_s: 30,
        };
        let out = client.transcribe_chunk("p", tmp.path()).await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    async fn proxy_client_omits_api_key_header() {
        let server = MockServer::start().await;
        // Server insists the x-goog-api-key header is ABSENT; wiremock doesn't have a
        // "header_absent" primitive, so instead install TWO mocks: one that matches
        // ONLY if the header is absent (returns 200), one fallback with header_exists
        // (returns 418 to flag a test failure).
        Mock::given(method("POST"))
            .and(header_exists("x-goog-api-key"))
            .respond_with(ResponseTemplate::new(418).set_body_string("api-key should be absent"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let out = client.transcribe_chunk("p", tmp.path()).await.unwrap();
        assert_eq!(out, "ok");
    }
}
