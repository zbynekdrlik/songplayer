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
//!
//! Retry policy: HTTP 429/500/503 are retried with exponential backoff (base
//! `base_retry_ms`, cap 60 s, max `max_attempts` total attempts).  If the
//! response carries a `Retry-After` header its value (seconds) is used instead
//! of the computed backoff.

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_THINKING_BUDGET: i32 = 2048;

/// HTTP statuses that are retried.
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 503];

pub struct GeminiClient {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub timeout_s: u64,
    /// Initial backoff delay in milliseconds. Doubles on each attempt, capped at 60 s.
    /// Override in tests to a small value (e.g. 10) so tests run fast.
    pub base_retry_ms: u64,
    /// Total number of attempts (first try + retries). Default: 4.
    pub max_attempts: u32,
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
            base_retry_ms: 10_000,
            max_attempts: 4,
        }
    }

    /// Proxy client. Calls a local CLIProxy (no auth header).
    pub fn proxy(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            timeout_s: 300,
            base_retry_ms: 10_000,
            max_attempts: 4,
        }
    }

    /// Send prompt + audio to Gemini, return the text body from the first candidate.
    ///
    /// Retries on HTTP 429/500/503 with exponential backoff. Honors `Retry-After`
    /// response header when present. Non-retryable errors (4xx other than 429) are
    /// returned immediately without retrying.
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

        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=self.max_attempts {
            let mut req = client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                req = req.header("x-goog-api-key", key.as_str());
            }
            let resp = req.send().await.context("POST to Gemini")?;
            let status = resp.status();
            let status_u16 = status.as_u16();

            if status.is_success() {
                let text = resp.text().await.context("read response body")?;
                let doc: serde_json::Value =
                    serde_json::from_str(&text).with_context(|| format!("parse JSON: {text}"))?;
                let out = doc
                    .pointer("/candidates/0/content/parts/0/text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("no text in candidates[0]: {text}"))?;
                return Ok(out.to_string());
            }

            // Check if this status is retryable.
            let is_retryable = RETRYABLE_STATUSES.contains(&status_u16);

            // Parse Retry-After header before consuming the response.
            let retry_after_ms: Option<u64> = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|secs| secs * 1000);

            let text = resp.text().await.context("read response body")?;
            let err_msg = format!("HTTP {status_u16}: {}", &text[..text.len().min(500)]);

            if !is_retryable || attempt >= self.max_attempts {
                anyhow::bail!("{err_msg}");
            }

            // Compute delay: Retry-After header takes precedence over computed backoff.
            let computed_backoff = (self.base_retry_ms * (1u64 << (attempt - 1))).min(60_000);
            let delay_ms = retry_after_ms.unwrap_or(computed_backoff);

            tracing::warn!(
                attempt,
                max = self.max_attempts,
                delay_ms,
                "gemini: retryable error ({err_msg}), sleeping before retry"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            last_error = Some(anyhow::anyhow!("{err_msg}"));
        }

        // Unreachable in practice (loop always returns or bails), but satisfies the compiler.
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("transcribe_chunk failed")))
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
        // Use a non-retryable status so the test completes in one attempt.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 10;
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("HTTP 400"), "err = {err}");
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
            base_retry_ms: 10,
            max_attempts: 4,
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

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let server = MockServer::start().await;
        let count = Arc::new(AtomicU32::new(0));
        let count_cloned = count.clone();
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(move |_: &wiremock::Request| {
                let n = count_cloned.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    ResponseTemplate::new(429).set_body_string("rate limited")
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "candidates": [{"content": {"parts": [{"text": "ok after retry"}]}}]
                    }))
                }
            })
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        // Shrink retry delays so the test runs fast
        client.base_retry_ms = 10;
        let out = client.transcribe_chunk("p", tmp.path()).await.unwrap();
        assert_eq!(out, "ok after retry");
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "expected 3 attempts (2 retries + final success)"
        );
    }

    #[tokio::test]
    async fn retries_on_503() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let server = MockServer::start().await;
        let count = Arc::new(AtomicU32::new(0));
        let count_cloned = count.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let n = count_cloned.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(503).set_body_string("busy")
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
                    }))
                }
            })
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 10;
        let _ = client.transcribe_chunk("p", tmp.path()).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 10;
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("HTTP 429"), "err = {err}");
    }

    #[tokio::test]
    async fn does_not_retry_on_400() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let server = MockServer::start().await;
        let count = Arc::new(AtomicU32::new(0));
        let count_cloned = count.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                count_cloned.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(400).set_body_string("bad request")
            })
            .mount(&server)
            .await;
        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 10;
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("HTTP 400"), "err = {err}");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "must not retry 4xx non-429"
        );
    }
}
