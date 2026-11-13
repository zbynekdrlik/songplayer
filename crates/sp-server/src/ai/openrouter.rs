//! #321: the ONE text-LLM client over OpenRouter's OpenAI-compatible chat
//! API (`POST {root}/chat/completions`, Bearer `openrouter_api_key`).
//!
//! - The request is the proxy client's shape (`model`, `messages` with an
//!   optional system message, `temperature`, `max_tokens`), so a task sends
//!   the same prompt whichever way it goes.
//! - The answer is decoded into typed structs (`ChatAnswer`), never through
//!   `serde_json::Value`'s own `Deserialize` (`rust-workspace.md`: the
//!   `raw_value` re-parse); a body that is no answer is an error naming only
//!   serde's line and column.
//! - A 429 / 5xx waits like the proxy's (`ai::retry::RetryPolicy`: its
//!   `Retry-After`, else 5 / 20 / 60 s); anything else fails at once.
//! - An error names the status and the first characters of the body with
//!   the key cut out ([`redacted_excerpt`]); the key is never logged, never
//!   in an error.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::retry::{RetryPolicy, body_excerpt, is_retried};

/// One completion's answer: its text and the model OpenRouter names (it may
/// name the exact version it routed to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub model: Option<String>,
}

/// What one call asks for.
#[derive(Debug, Clone)]
pub struct Ask<'a> {
    pub key: &'a str,
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub max_tokens: u32,
    pub timeout: Duration,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ChatAnswer {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    message: AnswerMessage,
}

#[derive(Deserialize)]
struct AnswerMessage {
    #[serde(default)]
    content: Option<String>,
}

/// The request body of a call (the proxy client's shape).
fn request_body<'a>(ask: &Ask<'a>) -> ChatRequest<'a> {
    let mut messages = Vec::with_capacity(2);
    if !ask.system.is_empty() {
        messages.push(Message {
            role: "system",
            content: ask.system,
        });
    }
    messages.push(Message {
        role: "user",
        content: ask.user,
    });
    ChatRequest {
        model: ask.model,
        messages,
        temperature: 0.1,
        max_tokens: ask.max_tokens,
    }
}

/// A 2xx body read as an answer: the first choice's text (none, or an
/// empty one, is an error) and the model it names.
pub fn read_answer(body: &str) -> Result<Completion> {
    let answer: ChatAnswer = serde_json::from_str(body).map_err(|e| {
        anyhow::anyhow!(
            "OpenRouter answer is not a chat completion (line {}, column {})",
            e.line(),
            e.column()
        )
    })?;
    let text = answer
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    if text.trim().is_empty() {
        anyhow::bail!("OpenRouter answered no text (choices[0].message.content)");
    }
    Ok(Completion {
        text,
        model: answer.model,
    })
}

/// The first [`super::retry::BODY_EXCERPT_CHARS`] characters of a refused
/// body, with every occurrence of `key` replaced FIRST (a key echoed across
/// the cut leaves no prefix). A blank key redacts nothing.
pub fn redacted_excerpt(body: &str, key: &str) -> String {
    let body = if key.is_empty() {
        body.to_string()
    } else {
        body.replace(key, "[REDACTED]")
    };
    body_excerpt(&body).replace('\n', " ")
}

/// The OpenRouter client: an API root and the waits of a refused call.
pub struct OpenRouterClient {
    http: reqwest::Client,
    api_url: String,
    retry: RetryPolicy,
}

impl OpenRouterClient {
    /// A client on `api_url` (`sp_core::ai_tasks::OPENROUTER_API_URL` in
    /// production, a mock in tests) with the production waits.
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_url: api_url.into(),
            retry: RetryPolicy::SPANNING,
        }
    }

    /// The same client with other waits (tests: `RetryPolicy::NO_WAIT`).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// One completion: retried on a 429 / 5xx, failed at once otherwise.
    pub async fn complete(&self, ask: &Ask<'_>) -> Result<Completion> {
        let url = format!("{}/chat/completions", self.api_url);
        let body = request_body(ask);
        for attempt in 0..self.retry.attempts() {
            debug!(attempt, model = ask.model, "openrouter: sending a chat completion");
            let resp = self
                .http
                .post(&url)
                .bearer_auth(ask.key)
                .timeout(ask.timeout)
                .json(&body)
                .send()
                .await
                .context("openrouter: the request was not sent")?;
            let status = resp.status();
            let delay = self.retry.after_response(status, resp.headers(), attempt + 1);
            let text = resp
                .text()
                .await
                .context("openrouter: the answer was not read")?;
            if status.is_success() {
                return read_answer(&text);
            }
            let excerpt = redacted_excerpt(&text, ask.key);
            match delay {
                Some(delay) if is_retried(status.as_u16()) => {
                    warn!(
                        status = %status,
                        retry = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        body = %excerpt,
                        model = ask.model,
                        "openrouter: refused - retrying after the wait"
                    );
                    tokio::time::sleep(delay).await;
                }
                _ => {
                    anyhow::bail!("OpenRouter refused (HTTP {status}): {excerpt}");
                }
            }
        }
        anyhow::bail!(
            "OpenRouter refused: all {} attempts",
            self.retry.attempts()
        )
    }
}

#[cfg(test)]
#[path = "openrouter_tests.rs"]
mod tests;
