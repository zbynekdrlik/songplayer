//! The text-AI client (#321): every text task's chat completion goes to
//! OpenRouter's OpenAI-compatible chat API (`ai/openrouter.rs`), with the
//! task's model, the key and the paid-AI switch read live from the database
//! at every call (`route.rs`, a child module). The owner's ruling (#232,
//! 13.11.2026, ROZHODNUTÉ on #321): the Claude-through-CLIProxyAPI path is
//! deleted, not kept as a fallback.

use sqlx::SqlitePool;

use super::openrouter::OpenRouterClient;
use super::retry::RetryPolicy;

// #321: each text task's model, read live per call, and the call itself.
#[path = "route.rs"]
pub mod route;

pub struct AiClient {
    /// The database the task models, the key and the paid-AI switch are
    /// read from at every call.
    pool: SqlitePool,
    openrouter: OpenRouterClient,
}

impl AiClient {
    /// The production client: OpenRouter at its public API root, the
    /// production waits ([`RetryPolicy::SPANNING`]).
    pub fn new(pool: SqlitePool) -> Self {
        Self::with_openrouter(
            pool,
            OpenRouterClient::new(sp_core::ai_tasks::OPENROUTER_API_URL),
        )
    }

    /// A client on another OpenRouter client (tests: a mock server).
    pub fn with_openrouter(pool: SqlitePool, openrouter: OpenRouterClient) -> Self {
        Self { pool, openrouter }
    }

    /// The same client with other waits between the attempts of a refused
    /// call (tests: `RetryPolicy::NO_WAIT`, so no test sleeps for real).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.openrouter = self.openrouter.with_retry_policy(retry);
        self
    }
}

/// Strip markdown code fences from LLM output.
///
/// Handles multiple cases:
/// 1. Entire response wrapped in ```json ... ```
/// 2. Preamble text followed by ```json ... ``` (a model often adds explanation)
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
}
