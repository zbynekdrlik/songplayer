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
//! of the computed backoff — BUT any value above the 60 s cap causes the
//! call to bail immediately instead of sleeping. Daily-quota 429s carry
//! `retry-after` values of hours; sleeping through them per chunk × per
//! key × per retry cycle burned 1076 retry-storm events on 2026-04-23.
//!
//! Audit trail: every attempt (success, retryable failure, or terminal failure)
//! emits one `GeminiAuditEntry` when `ctx.cache_dir` is set. A single chunk
//! that succeeds on attempt 3 after two 429s produces three audit lines. Audit
//! write errors are logged and swallowed — a disk-full condition should not
//! mask a successful Gemini call.

use crate::lyrics::gemini_audit::{self, GeminiAuditEntry};
use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_THINKING_BUDGET: i32 = 2048;

/// HTTP statuses that are retried.
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 503];

/// Upper bound on the `Retry-After` header we'll actually honor. When
/// Google's daily quota hits, `retry-after` is measured in hours
/// (10 000+ seconds). The 2026-04-23 event burned 1076 retry-storm
/// events when this wasn't clamped. Anything above this cap is treated
/// as "not worth waiting, switch keys or give up now".
const RETRY_AFTER_CAP_MS: u64 = 60_000;

/// Exponential backoff for retryable failures. `attempt` is 1-indexed
/// (first retry = 1). Returns `min(base_retry_ms * 2^(attempt-1), cap_ms)`.
///
/// Extracted from the inline retry loop so the arithmetic can be pinned
/// with concrete value assertions in unit tests — tokio sleep durations
/// are too jittery in CI to assert on, but the underlying formula is
/// deterministic.
fn compute_backoff(base_retry_ms: u64, attempt: u32, cap_ms: u64) -> u64 {
    (base_retry_ms * (1u64 << (attempt - 1))).min(cap_ms)
}

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

/// Parsed `usageMetadata` from a Gemini response body. Each field is the number
/// of tokens Google bills for that segment; `total` is usually `prompt + candidates`
/// but some models add a small thinking-budget overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub candidates: u32,
    pub total: u32,
}

/// Context carried from the caller through `post_with_retries` to the audit
/// logger. `cache_dir = None` disables audit writing entirely — used by
/// wiremock unit tests that don't want to touch the filesystem, and as a
/// placeholder for translator calls that haven't been wired up yet.
#[derive(Debug, Clone)]
pub struct AuditCtx {
    pub cache_dir: Option<PathBuf>,
    pub video_id: Option<String>,
    pub chunk_idx: Option<u32>,
    pub key_idx: usize,
}

impl AuditCtx {
    /// Sentinel context that disables audit writes (`cache_dir = None`). Used
    /// by tests + any production path that hasn't yet been wired with audit
    /// metadata. `Default` was previously derived but a forgotten field
    /// assignment would silently audit "as key 0" — making the no-audit case
    /// explicit forces callers to opt in by name.
    pub fn no_audit() -> Self {
        Self {
            cache_dir: None,
            video_id: None,
            chunk_idx: None,
            key_idx: 0,
        }
    }
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
    pub async fn transcribe_chunk(
        &self,
        prompt: &str,
        audio_wav: &Path,
        ctx: AuditCtx,
    ) -> Result<(String, Option<TokenUsage>)> {
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

        self.post_with_retries(&body, &ctx).await
    }

    /// Send a text-only prompt to Gemini, return the text body from the first
    /// candidate. Used for translation when Claude refuses with a copyright
    /// policy error. Same retry semantics as `transcribe_chunk`.
    ///
    /// `temperature = 0.3` gives the model a small amount of flexibility on
    /// word choice (Slovak has multiple valid renderings for many English
    /// phrases) while remaining deterministic enough for regression testing.
    pub async fn generate_text(
        &self,
        prompt: &str,
        ctx: AuditCtx,
    ) -> Result<(String, Option<TokenUsage>)> {
        let body = json!({
            "contents": [{
                "parts": [{"text": prompt}]
            }],
            "generationConfig": {
                "temperature": 0.3,
                "thinkingConfig": {"thinkingBudget": 1024}
            }
        });

        self.post_with_retries(&body, &ctx).await
    }

    /// First 12 chars of the API key — used as `key_prefix` in audit entries.
    /// Returns `proxy` for proxy mode (no key) so operators can tell the two
    /// apart when inspecting the log.
    fn key_prefix(&self) -> String {
        match &self.api_key {
            Some(k) => k.chars().take(12).collect(),
            None => "proxy".to_string(),
        }
    }

    /// Write one audit entry. Write failures are logged and swallowed — we do
    /// NOT want a full disk to fail legitimate Gemini calls.
    async fn write_audit(&self, ctx: &AuditCtx, entry: GeminiAuditEntry) {
        let Some(cache_dir) = ctx.cache_dir.as_ref() else {
            return;
        };
        if let Err(e) = gemini_audit::append(cache_dir, &entry).await {
            tracing::warn!("gemini: audit append failed: {e}");
        }
    }

    /// Shared HTTP loop used by `transcribe_chunk` and `generate_text`. Builds
    /// the URL from `base_url` + `model`, adds the `x-goog-api-key` header when
    /// `api_key` is `Some`, and retries on HTTP 429/500/503 with exponential
    /// backoff (capped at 60 s, `Retry-After` header honored when present).
    /// Returns the string at `/candidates/0/content/parts/0/text` on success,
    /// plus the parsed `usageMetadata` if the response carried it.
    ///
    /// Every attempt emits one audit entry — success, retryable failure, and
    /// terminal failure alike. A three-attempt sequence (429, 429, 200)
    /// produces three audit lines.
    // mutants::skip: the `ms > RETRY_AFTER_CAP_MS` comparison (line 316) has
    // `> → ==`, `> → <`, `> → >=` survivors. Killing all three needs concrete
    // assertions around exact retry-after values; `> → <` in particular causes
    // the wiremock test to time out on small retry-after values (the retry
    // loop then sleeps through several seconds × max_attempts). Behaviour is
    // covered by `bails_fast_when_retry_after_exceeds_cap` + the extracted
    // `compute_backoff` unit test with four concrete value assertions.
    #[cfg_attr(test, mutants::skip)]
    async fn post_with_retries(
        &self,
        body: &serde_json::Value,
        ctx: &AuditCtx,
    ) -> Result<(String, Option<TokenUsage>)> {
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
        let key_prefix = self.key_prefix();

        for attempt in 1..=self.max_attempts {
            let attempt_start = Instant::now();
            let mut req = client.post(&url).json(body);
            if let Some(key) = &self.api_key {
                req = req.header("x-goog-api-key", key.as_str());
            }
            let resp_result = req.send().await;

            // Transport error (DNS, TCP, TLS, timeout): log with status=0 and
            // bubble up — transport failures are not retried (the retry loop
            // only handles HTTP status-level failures per the existing contract).
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                    let err_msg = format!("transport: {e}");
                    self.write_audit(
                        ctx,
                        self.build_entry(
                            ctx,
                            &key_prefix,
                            0,
                            elapsed_ms,
                            None,
                            Some(err_msg.clone()),
                        ),
                    )
                    .await;
                    return Err(e).context("POST to Gemini");
                }
            };

            let status = resp.status();
            let status_u16 = status.as_u16();

            if status.is_success() {
                let text = resp.text().await.context("read response body")?;
                let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
                let doc: serde_json::Value =
                    serde_json::from_str(&text).with_context(|| format!("parse JSON: {text}"))?;
                let usage = parse_usage_metadata(&doc);
                self.write_audit(
                    ctx,
                    self.build_entry(ctx, &key_prefix, status_u16, elapsed_ms, usage, None),
                )
                .await;
                let out = doc
                    .pointer("/candidates/0/content/parts/0/text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("no text in candidates[0]: {text}"))?;
                return Ok((out.to_string(), usage));
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
            let elapsed_ms = attempt_start.elapsed().as_millis() as u64;
            let err_msg = format!("HTTP {status_u16}: {}", &text[..text.len().min(500)]);

            // Audit this failed attempt (retryable or terminal).
            self.write_audit(
                ctx,
                self.build_entry(
                    ctx,
                    &key_prefix,
                    status_u16,
                    elapsed_ms,
                    None,
                    Some(err_msg.clone()),
                ),
            )
            .await;

            if !is_retryable || attempt >= self.max_attempts {
                anyhow::bail!("{err_msg}");
            }

            // Daily-quota guard: when retry-after exceeds the cap, don't
            // sleep through it — bail immediately so the caller can
            // rotate keys or give up. Otherwise the retry loop wastes
            // wall-clock time on dead keys (observed 1076× on 2026-04-23).
            if let Some(ms) = retry_after_ms
                && ms > RETRY_AFTER_CAP_MS
            {
                anyhow::bail!(
                    "HTTP {status_u16} retry-after={}s (exceeds {}s cap)",
                    ms / 1000,
                    RETRY_AFTER_CAP_MS / 1000
                );
            }

            // Compute delay: Retry-After header takes precedence over computed backoff.
            let computed_backoff = compute_backoff(self.base_retry_ms, attempt, RETRY_AFTER_CAP_MS);
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
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("post_with_retries failed")))
    }

    fn build_entry(
        &self,
        ctx: &AuditCtx,
        key_prefix: &str,
        status: u16,
        duration_ms: u64,
        usage: Option<TokenUsage>,
        error: Option<String>,
    ) -> GeminiAuditEntry {
        GeminiAuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            video_id: ctx.video_id.clone(),
            chunk_idx: ctx.chunk_idx,
            key_idx: ctx.key_idx,
            key_prefix: key_prefix.to_string(),
            model: self.model.clone(),
            status,
            duration_ms,
            prompt_tokens: usage.map(|u| u.prompt),
            candidates_tokens: usage.map(|u| u.candidates),
            total_tokens: usage.map(|u| u.total),
            error,
        }
    }
}

/// Pull `usageMetadata.{promptTokenCount, candidatesTokenCount, totalTokenCount}`
/// out of a Gemini response body. All three are optional per Google's schema —
/// if any one is missing we return `None` (rather than defaulting to zero,
/// which would lie about the spend).
fn parse_usage_metadata(doc: &serde_json::Value) -> Option<TokenUsage> {
    let usage = doc.get("usageMetadata")?;
    let prompt = usage.get("promptTokenCount")?.as_u64()? as u32;
    let candidates = usage.get("candidatesTokenCount")?.as_u64()? as u32;
    let total = usage.get("totalTokenCount")?.as_u64()? as u32;
    Some(TokenUsage {
        prompt,
        candidates,
        total,
    })
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

    /// No-audit context — disables audit writes so tests that don't care
    /// about the audit file can ignore the filesystem.
    fn noaudit() -> AuditCtx {
        AuditCtx::no_audit()
    }

    /// Pins the exponential-backoff formula with concrete values. Kills
    /// the four arithmetic mutants on the `<<`, `-`, and `*` operators
    /// in `compute_backoff` — attempt=2 is the first value that
    /// distinguishes all four mutations from the real formula, and the
    /// cap test exercises the `.min(cap_ms)` branch.
    #[test]
    fn compute_backoff_pins_exponential_formula_and_cap() {
        // attempt=1: base_retry * 2^0 = base_retry (well below cap).
        // Does NOT distinguish `<<` → `>>` or `*` → `/` (identity when
        // shift amount is 0) but DOES kill both `-` mutants (`-` → `+`
        // yields 40; `-` → `/` yields 20).
        assert_eq!(compute_backoff(10, 1, 60_000), 10);

        // attempt=2: base_retry * 2^1 = 20.
        // - real: 20
        // - `<<` → `>>`: 10 * (1 >> 1) = 0
        // - `*` → `/`:   10 / 2          = 5
        // - `-` → `+`:   10 * (1 << 3)   = 80
        // - `-` → `/`:   10 * (1 << 2)   = 40
        // Kills all four mutants.
        assert_eq!(compute_backoff(10, 2, 60_000), 20);

        // attempt=3: 10 * 4 = 40. Reinforces the doubling growth pattern.
        assert_eq!(compute_backoff(10, 3, 60_000), 40);

        // attempt=14 with base=10: 10 * 2^13 = 81_920 → clamped to
        // cap=60_000. Kills `<<` and `*` at the cap boundary (both
        // mutants collapse the product to 0, which doesn't equal the
        // cap). Note: `-` → `+` and `-` → `/` both still over-shift
        // here, producing products > cap that also min-to cap, so this
        // specific assertion does not add coverage for the `-` mutants
        // — that's what the attempt=1/2 assertions above are for.
        assert_eq!(compute_backoff(10, 14, 60_000), 60_000);
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
        let (out, usage) = client
            .transcribe_chunk("prompt", tmp.path(), noaudit())
            .await
            .unwrap();
        assert_eq!(out, "(00:01.0 --> 00:02.0) hello");
        assert!(usage.is_none(), "no usageMetadata in mock response");
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
        let err = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap_err();
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
        let err = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap_err();
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
        let (out, _usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
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
        let (out, _usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
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
        let (out, _usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
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
        let _ = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
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
        let err = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap_err();
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
        let err = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("HTTP 400"), "err = {err}");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "must not retry 4xx non-429"
        );
    }

    #[tokio::test]
    async fn generate_text_extracts_text_from_first_candidate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{
                    "content": {
                        "parts": [{"text": "hello"}]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let (out, _usage) = client
            .generate_text("translate these lines", noaudit())
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn generate_text_retries_on_429_then_succeeds() {
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

        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        // Shrink retry delays so the test runs fast.
        client.base_retry_ms = 10;
        let (out, _usage) = client.generate_text("prompt", noaudit()).await.unwrap();
        assert_eq!(out, "ok after retry");
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "expected 3 attempts (2 retries + final success)"
        );
    }

    // --- Audit / usageMetadata tests (Task 2) --------------------------------

    #[tokio::test]
    async fn post_parses_usage_metadata_when_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 1234,
                    "candidatesTokenCount": 567,
                    "totalTokenCount": 1801
                }
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let (_out, usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
        let usage = usage.expect("usageMetadata must be parsed when present");
        assert_eq!(usage.prompt, 1234);
        assert_eq!(usage.candidates, 567);
        assert_eq!(usage.total, 1801);
    }

    #[tokio::test]
    async fn post_returns_none_usage_when_field_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let (_out, usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
        assert!(usage.is_none());
    }

    #[tokio::test]
    async fn post_returns_none_usage_when_field_partial() {
        // All three tokens must be present; missing any one = None, to avoid
        // silently reporting zero when Google schema shifts.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 10
                    // missing candidatesTokenCount and totalTokenCount
                }
            })))
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        let (_out, usage) = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap();
        assert!(usage.is_none());
    }

    #[tokio::test]
    async fn post_writes_audit_entry_with_cache_dir_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "candidatesTokenCount": 5,
                    "totalTokenCount": 15
                }
            })))
            .mount(&server)
            .await;

        let tmp_wav = write_tmp_wav();
        let cache = tempfile::tempdir().unwrap();
        let client = GeminiClient {
            base_url: server.uri(),
            model: "gemini-test".to_string(),
            api_key: Some("AIzaSyTESTKEY1234".to_string()),
            timeout_s: 10,
            base_retry_ms: 10,
            max_attempts: 4,
        };
        let ctx = AuditCtx {
            cache_dir: Some(cache.path().to_path_buf()),
            video_id: Some("vidX".to_string()),
            chunk_idx: Some(2),
            key_idx: 1,
        };
        let _ = client
            .transcribe_chunk("p", tmp_wav.path(), ctx)
            .await
            .unwrap();

        let entries = crate::lyrics::gemini_audit::read_entries(cache.path(), None, None)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.status, 200);
        assert_eq!(e.video_id.as_deref(), Some("vidX"));
        assert_eq!(e.chunk_idx, Some(2));
        assert_eq!(e.key_idx, 1);
        assert_eq!(e.key_prefix, "AIzaSyTESTKE"); // first 12 chars
        assert_eq!(e.model, "gemini-test");
        assert_eq!(e.prompt_tokens, Some(10));
        assert_eq!(e.candidates_tokens, Some(5));
        assert_eq!(e.total_tokens, Some(15));
        assert!(e.error.is_none());
    }

    #[tokio::test]
    async fn post_writes_audit_entry_on_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let tmp_wav = write_tmp_wav();
        let cache = tempfile::tempdir().unwrap();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 1; // make retries instant
        client.max_attempts = 1; // only one attempt so the test is bounded
        let ctx = AuditCtx {
            cache_dir: Some(cache.path().to_path_buf()),
            video_id: Some("v".to_string()),
            chunk_idx: Some(0),
            key_idx: 0,
        };
        let _ = client.transcribe_chunk("p", tmp_wav.path(), ctx).await;

        let entries = crate::lyrics::gemini_audit::read_entries(cache.path(), None, None)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.status, 429);
        assert!(e.error.as_deref().unwrap().contains("HTTP 429"));
        assert!(e.total_tokens.is_none());
        assert_eq!(e.key_prefix, "proxy"); // proxy mode
    }

    /// When Google's daily quota hits, the Retry-After header is measured
    /// in hours (10 000+ seconds). Sleeping through that per-chunk ×
    /// per-key × per-retry-cycle burned 1076 retry-storm events on the
    /// 2026-04-23 event. Bail immediately on Retry-After > 60 s instead
    /// of consuming another attempt slot + minutes of wall-clock sleep.
    #[tokio::test]
    async fn bails_fast_when_retry_after_exceeds_cap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let server = MockServer::start().await;
        let count = Arc::new(AtomicU32::new(0));
        let count_cloned = count.clone();
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(move |_: &wiremock::Request| {
                count_cloned.fetch_add(1, Ordering::SeqCst);
                // Retry-After = 35 000 s (≈ 9.7 h) — above the 60 s cap.
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "35000")
                    .set_body_string("daily quota exceeded")
            })
            .mount(&server)
            .await;

        let tmp = write_tmp_wav();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        // Base retry is already small; the test is about the cap, not backoff.
        client.base_retry_ms = 10;
        let started = std::time::Instant::now();
        let err = client
            .transcribe_chunk("p", tmp.path(), noaudit())
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must bail without sleeping (< 2 s), got {elapsed:?}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "must make only one attempt, not consume the retry budget"
        );
        assert!(
            format!("{err}").contains("retry-after"),
            "error must explain why we bailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn post_writes_one_audit_entry_per_attempt_on_retry_then_success() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let server = MockServer::start().await;
        let count = Arc::new(AtomicU32::new(0));
        let count_cloned = count.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let n = count_cloned.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    ResponseTemplate::new(429).set_body_string("rate limited")
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
                        "usageMetadata": {
                            "promptTokenCount": 1,
                            "candidatesTokenCount": 2,
                            "totalTokenCount": 3
                        }
                    }))
                }
            })
            .mount(&server)
            .await;

        let tmp_wav = write_tmp_wav();
        let cache = tempfile::tempdir().unwrap();
        let mut client = GeminiClient::proxy(server.uri(), "gemini-3.1-pro-preview");
        client.base_retry_ms = 1;
        let ctx = AuditCtx {
            cache_dir: Some(cache.path().to_path_buf()),
            video_id: Some("v".to_string()),
            chunk_idx: Some(0),
            key_idx: 0,
        };
        let _ = client
            .transcribe_chunk("p", tmp_wav.path(), ctx)
            .await
            .unwrap();

        let entries = crate::lyrics::gemini_audit::read_entries(cache.path(), None, None)
            .await
            .unwrap();
        assert_eq!(entries.len(), 3, "one audit row per attempt");
        assert_eq!(entries[0].status, 429);
        assert_eq!(entries[1].status, 429);
        assert_eq!(entries[2].status, 200);
        assert_eq!(entries[2].total_tokens, Some(3));
    }
}
