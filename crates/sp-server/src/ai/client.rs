//! OpenAI-compatible HTTP client for CLIProxyAPI.
//!
//! Sends chat completion requests to the local CLIProxyAPI proxy,
//! which forwards them to Claude Opus.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use super::AiSettings;
use super::retry::{RetryPolicy, body_excerpt, is_content_filtered, is_retried};

pub struct AiClient {
    http: reqwest::Client,
    settings: AiSettings,
    /// #145: the waits between the attempts of a refused call.
    retry: RetryPolicy,
}

impl AiClient {
    /// A client with the production retry policy
    /// ([`RetryPolicy::SPANNING`], `ai/retry.rs`).
    pub fn new(settings: AiSettings) -> Self {
        Self {
            http: reqwest::Client::new(),
            settings,
            retry: RetryPolicy::SPANNING,
        }
    }

    /// The same client with another retry policy: a caller that cannot wait
    /// out the proxy's cooldown, and the tests (`RetryPolicy::NO_WAIT`, so no
    /// test sleeps for real).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Send a chat completion and return the assistant's raw text response.
    /// Uses a default 300-second timeout — sufficient for typical requests.
    /// For large merge prompts use [`chat_with_timeout`] with a higher value.
    #[cfg_attr(test, mutants::skip)]
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        self.chat_with_timeout(system, user, 300).await
    }

    /// Send a chat completion with a caller-specified per-request timeout in seconds.
    ///
    /// This exists because LLM merge prompts for long songs (100+ lines, two providers)
    /// can exceed the default 300s timeout: Claude Opus via CLIProxyAPI produces output
    /// at ~50 tok/sec, so a 500-token response takes ~10s plus network, but larger songs
    /// with 14k+ char prompts can push well past 300s. Callers that need more time (e.g.
    /// `merge_provider_results`) should pass 600.
    #[cfg_attr(test, mutants::skip)]
    pub async fn chat_with_timeout(
        &self,
        system: &str,
        user: &str,
        timeout_secs: u64,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.settings.api_url);

        let mut messages = Vec::new();
        if !system.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        messages.push(serde_json::json!({"role": "user", "content": user}));

        let body = serde_json::json!({
            "model": self.settings.model,
            "messages": messages,
            "temperature": 0.1,
            "max_tokens": 32000
        });

        // #145: a 429 / 5xx is retried after its `Retry-After`, else after
        // the policy's spanning waits (`ai/retry.rs`); `attempt` counts the
        // retries (0 = the first request). The loop is bounded by the
        // policy's `attempts()` on its own, never only by `after_response`.
        for attempt in 0..self.retry.attempts() {
            debug!(attempt, url = %url, "sending chat completion request");

            let mut req = self.http.post(&url).json(&body);
            if let Some(ref key) = self.settings.api_key {
                req = req.header("Authorization", format!("Bearer {key}"));
            }

            let resp = req
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .send()
                .await
                .context("failed to send chat completion request")?;

            let status = resp.status();
            if status.is_success() {
                let json: serde_json::Value = resp
                    .json()
                    .await
                    .context("failed to parse chat completion response")?;
                let content = json["choices"][0]["message"]["content"]
                    .as_str()
                    .ok_or_else(|| {
                        anyhow::anyhow!("missing choices[0].message.content in response")
                    })?
                    .to_string();
                return Ok(content);
            }

            let retry = attempt + 1;
            let delay = self.retry.after_response(status, resp.headers(), retry);
            let body_text = resp.text().await.unwrap_or_default();
            // #144: final — the same output is refused again, and each
            // refusal puts the proxy's credential into its cooldown.
            if is_content_filtered(&body_text) {
                warn!(
                    status = %status,
                    body = %body_excerpt(&body_text),
                    "chat completion refused by the upstream content filter — final, not retried"
                );
                anyhow::bail!("chat completion failed (HTTP {status}): {body_text}");
            }
            if is_retried(status.as_u16()) {
                log_refusal(status, retry, delay, &body_text);
            }
            let Some(delay) = delay else {
                anyhow::bail!("chat completion failed (HTTP {status}): {body_text}");
            };
            tokio::time::sleep(delay).await;
        }
        anyhow::bail!(
            "chat completion failed: all {} attempts refused",
            self.retry.attempts()
        )
    }

    /// Send a chat completion and parse the response as JSON.
    ///
    /// The LLM response may contain markdown code fences — strip them
    /// before parsing.
    #[cfg_attr(test, mutants::skip)]
    pub async fn chat_json<T: DeserializeOwned>(&self, system: &str, user: &str) -> Result<T> {
        let raw = self.chat(system, user).await?;
        let cleaned = strip_markdown_fences(&raw);
        serde_json::from_str(&cleaned)
            .with_context(|| format!("failed to parse LLM response as JSON: {cleaned}"))
    }

    /// Access the underlying settings.
    #[cfg_attr(test, mutants::skip)]
    pub fn settings(&self) -> &AiSettings {
        &self.settings
    }
}

/// #145: the WARN of a call the proxy refused with a 429 / 5xx — the status,
/// which retry comes next (or none), the wait, and the first 300 characters
/// of the body, so a refusal's reason (`auth_unavailable`, `model_cooldown`,
/// the upstream's text) is in the log. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_refusal(
    status: reqwest::StatusCode,
    retry: u32,
    delay: Option<std::time::Duration>,
    body: &str,
) {
    match delay {
        Some(delay) => warn!(
            status = %status,
            retry,
            delay_ms = delay.as_millis() as u64,
            body = %body_excerpt(body),
            "chat completion refused — retrying after the wait"
        ),
        None => warn!(
            status = %status,
            retries = retry - 1,
            body = %body_excerpt(body),
            "chat completion refused — no retry left"
        ),
    }
}

/// Strip markdown code fences from LLM output.
///
/// Handles multiple cases:
/// 1. Entire response wrapped in ```json ... ```
/// 2. Preamble text followed by ```json ... ``` (Claude often adds explanation)
/// 3. No fences — return as-is
#[cfg_attr(test, mutants::skip)]
pub fn strip_markdown_fences(s: &str) -> String {
    let trimmed = s.trim();

    // Find ``` anywhere in the string (not just at the start)
    if let Some(fence_start) = trimmed.find("```") {
        let after_fence = &trimmed[fence_start + 3..];
        // Skip optional language tag on the first line
        let content_start = if let Some(newline_pos) = after_fence.find('\n') {
            &after_fence[newline_pos + 1..]
        } else {
            after_fence
        };
        // Find closing fence
        if let Some(close_pos) = content_start.find("```") {
            return content_start[..close_pos].trim().to_string();
        }
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::retry::RetryPolicy;

    #[test]
    fn parse_openai_response() {
        let response_json = r#"{
            "choices": [{
                "message": {
                    "content": "{\"result\": \"hello\"}"
                }
            }]
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(response_json).unwrap();
        let content = parsed["choices"][0]["message"]["content"].as_str().unwrap();
        assert_eq!(content, r#"{"result": "hello"}"#);
    }

    #[test]
    fn ai_settings_default() {
        let s = AiSettings::default();
        assert_eq!(s.api_url, "http://localhost:18787/v1");
        assert!(s.model.contains("claude"));
    }

    #[test]
    fn strip_markdown_fences_json() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fences(input), r#"{"key": "value"}"#);
    }

    #[test]
    fn strip_markdown_fences_plain() {
        let input = "```\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fences(input), r#"{"key": "value"}"#);
    }

    #[test]
    fn strip_markdown_fences_no_fences() {
        let input = r#"{"key": "value"}"#;
        assert_eq!(strip_markdown_fences(input), input);
    }

    #[test]
    fn strip_markdown_fences_with_whitespace() {
        let input = "  ```json\n  {\"key\": \"value\"}  \n```  ";
        let result = strip_markdown_fences(input);
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn strip_markdown_fences_with_preamble() {
        // Claude often adds explanation text before the JSON block
        let input = "I'll analyze the data and produce the merged result.\n\n```json\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fences(input), r#"{"key": "value"}"#);
    }

    #[test]
    fn strip_markdown_fences_with_preamble_and_trailing() {
        let input =
            "Here is the result:\n\n```json\n{\"lines\": []}\n```\n\nThe merge is complete.";
        assert_eq!(strip_markdown_fences(input), r#"{"lines": []}"#);
    }

    #[tokio::test]
    async fn chat_success_returns_content() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hello world"}}]
            })))
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        });
        let result = client.chat("sys", "user").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn chat_retries_on_5xx_and_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First call: 503, second: 200
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "retry succeeded"}}]
            })))
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        })
        .with_retry_policy(RetryPolicy::NO_WAIT);
        let result = client.chat("", "user").await.unwrap();
        assert_eq!(result, "retry succeeded");
    }

    /// #145: a proxy that keeps refusing (a 503 inside its cooldown, with a
    /// `Retry-After`) gets the first request and the policy's 3 retries, then
    /// the call fails with the refusal's body.
    #[tokio::test]
    async fn chat_gives_up_after_its_retries_with_the_refusal_s_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("Retry-After", "55")
                    .set_body_string(
                        r#"{"error":{"code":"auth_unavailable","message":"no auth available"}}"#,
                    ),
            )
            .expect(4)
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        })
        .with_retry_policy(RetryPolicy::NO_WAIT);
        let err = client.chat("", "user").await.unwrap_err().to_string();
        assert!(err.contains("HTTP 503"), "{err}");
        assert!(err.contains("auth_unavailable"), "{err}");
    }

    /// #144: Claude's upstream content filter refuses the same output every
    /// time, and each refusal puts the proxy's one credential into its
    /// cooldown (every AI call refused for ~60 s): it is final, never retried.
    #[tokio::test]
    async fn chat_does_not_retry_a_content_filter_refusal() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_string(
                r#"{"error":{"message":"claude executor: upstream returned error event: Output blocked by content filtering policy","type":"server_error"}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        })
        .with_retry_policy(RetryPolicy::NO_WAIT);
        let err = client.chat("", "user").await.unwrap_err().to_string();
        assert!(err.contains("content filtering policy"), "{err}");
    }

    #[tokio::test]
    async fn chat_fails_on_400_without_retry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1) // no retry for 400
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        });
        let result = client.chat("", "user").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_with_timeout_respects_custom_timeout() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(2))
                    .set_body_json(serde_json::json!({
                        "choices": [{"message": {"content": "ok"}}]
                    })),
            )
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: Some("t".into()),
            model: "m".into(),
            system_prompt_extra: None,
        });

        let short = client.chat_with_timeout("", "q", 1).await;
        assert!(short.is_err(), "1s timeout against 2s mock must fail");

        let long = client.chat_with_timeout("", "q", 5).await;
        assert!(
            long.is_ok(),
            "5s timeout against 2s mock must succeed, got {long:?}"
        );
    }

    #[tokio::test]
    async fn chat_json_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "```json\n{\"ok\": true, \"count\": 42}\n```"}}]
            })))
            .mount(&server)
            .await;

        #[derive(serde::Deserialize)]
        struct Reply {
            ok: bool,
            count: u32,
        }

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        });
        let result: Reply = client.chat_json("", "user").await.unwrap();
        assert!(result.ok);
        assert_eq!(result.count, 42);
    }
}
