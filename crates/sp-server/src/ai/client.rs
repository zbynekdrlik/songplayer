//! OpenAI-compatible HTTP client for CLIProxyAPI.
//!
//! Sends chat completion requests to the local CLIProxyAPI proxy,
//! which forwards them to Claude Opus.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use super::AiSettings;

pub struct AiClient {
    http: reqwest::Client,
    settings: AiSettings,
}

impl AiClient {
    pub fn new(settings: AiSettings) -> Self {
        Self {
            http: reqwest::Client::new(),
            settings,
        }
    }

    /// Send a chat completion and return the assistant's raw text response.
    #[cfg_attr(test, mutants::skip)]
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
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

        // Retry with exponential backoff on 429/5xx
        let mut attempt = 0;
        let max_retries = 3;
        loop {
            attempt += 1;
            debug!(attempt, url = %url, "sending chat completion request");

            let mut req = self.http.post(&url).json(&body);
            if let Some(ref key) = self.settings.api_key {
                req = req.header("Authorization", format!("Bearer {key}"));
            }

            let resp = req
                .timeout(std::time::Duration::from_secs(300))
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

            if attempt >= max_retries || !(status.as_u16() == 429 || status.is_server_error()) {
                let body_text = resp.text().await.unwrap_or_default();
                anyhow::bail!("chat completion failed (HTTP {status}): {body_text}");
            }

            let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt as u32 - 1));
            warn!(status = %status, ?delay, attempt, "retrying chat completion");
            tokio::time::sleep(delay).await;
        }
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
        });
        let result = client.chat("", "user").await.unwrap();
        assert_eq!(result, "retry succeeded");
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
