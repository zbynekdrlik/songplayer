//! How the AI client waits between the attempts of a call the proxy refused
//! with a 429 or a 5xx (#145, design record 5908861002).
//!
//! CLIProxyAPI (8.0.4, source read on #145) puts the one OAuth credential
//! into a cooldown after an upstream 5xx — 60 s by default — and while it
//! cools it refuses every request LOCALLY with a 503 `auth_unavailable`
//! carrying `Retry-After` = the cooldown left. The old 1 s / 2 s retries
//! landed inside that cooldown every time (503s in 4-15 ms, box log
//! 30.9.2026), so a short upstream blip failed a Claude cleanup outright.
//!
//! - A `Retry-After` in seconds is honoured, capped at
//!   [`RetryPolicy::retry_after_cap`] (120 s): the next attempt lands right
//!   after the cooldown.
//! - Without one (the upstream's 5xx relayed, an older proxy, cooling
//!   disabled) the waits are [`RetryPolicy::fallback`] (5 s, 20 s, 60 s; its
//!   length is the retry budget): 85 s in all outlasts the default 60 s
//!   cooldown.
//! - Only 429 and 5xx are retried; anything else fails at once. So does a
//!   refusal by Claude's upstream content filter (#144,
//!   [`is_content_filtered`]): the same output is refused again, and each
//!   refusal costs the proxy's cooldown — ~60 s of every AI call refused.
//!
//! Every caller today is background work (lyrics, translation, metadata), so
//! the client uses [`RetryPolicy::SPANNING`]; a caller that cannot wait sets
//! its own policy (`AiClient::with_retry_policy`), and so do the tests, which
//! never sleep for real.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};

/// How much of a refused call's body goes to the log (characters).
pub const BODY_EXCERPT_CHARS: usize = 300;

/// The waits of a call's retries (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The wait before retry `n` (1-based) when the answer names none; its
    /// length is the retry budget.
    pub fallback: &'static [Duration],
    /// The longest `Retry-After` honoured.
    pub retry_after_cap: Duration,
}

impl RetryPolicy {
    /// Production: 3 retries after 5 s, 20 s, 60 s, or after the proxy's
    /// `Retry-After` up to 120 s (the module doc).
    pub const SPANNING: Self = Self {
        fallback: &[
            Duration::from_secs(5),
            Duration::from_secs(20),
            Duration::from_secs(60),
        ],
        retry_after_cap: Duration::from_secs(120),
    };

    /// Tests: the production budget with no waits at all.
    #[cfg(test)]
    pub const NO_WAIT: Self = Self {
        fallback: &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
        retry_after_cap: Duration::ZERO,
    };

    /// The wait before retry `attempt` (1 = the first retry) of a call
    /// answered `status`, whose `Retry-After` header was `retry_after`:
    /// `None` when the status is not retried (only 429 and 5xx are) or the
    /// retries are spent. A `Retry-After` of whole seconds wins over the
    /// fallback, capped at `retry_after_cap`; any other form (an HTTP date,
    /// garbage) falls back.
    pub fn retry_delay(
        &self,
        status: u16,
        retry_after: Option<&str>,
        attempt: u32,
    ) -> Option<Duration> {
        if !is_retried(status) {
            return None;
        }
        let fallback = *self.fallback.get(attempt.checked_sub(1)? as usize)?;
        let named = retry_after.and_then(|value| value.trim().parse::<u64>().ok());
        Some(named.map_or(fallback, |secs| {
            Duration::from_secs(secs).min(self.retry_after_cap)
        }))
    }

    /// The most requests one call makes: the first plus one per fallback
    /// wait. The client's loop is bounded by this on its own, so no answer
    /// of [`Self::retry_delay`] can make a call retry forever.
    pub fn attempts(&self) -> u32 {
        self.fallback.len() as u32 + 1
    }

    /// [`Self::retry_delay`] for a response's status and headers.
    pub fn after_response(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        attempt: u32,
    ) -> Option<Duration> {
        let retry_after = headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok());
        self.retry_delay(status.as_u16(), retry_after, attempt)
    }
}

/// Whether a status is retried: 429 or any 5xx.
pub fn is_retried(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// #144: whether a refused call's body is Claude's upstream content filter
/// ("Output blocked by content filtering policy", relayed by CLIProxyAPI as
/// a 502). The same request is refused again, and every refusal puts the
/// proxy's one credential into its cooldown, so the client never retries
/// it.
pub fn is_content_filtered(body: &str) -> bool {
    body.contains("content filtering policy")
}

/// #144: a call Claude's upstream content filter refused
/// ([`is_content_filtered`]): the client's error for it, typed so a caller
/// asks [`content_filtered`] by type, never by an error's text.
#[derive(Debug)]
pub struct ContentFiltered {
    /// The refusal: its status and the proxy's body (or why it was not sent).
    pub detail: String,
}

impl std::fmt::Display for ContentFiltered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "chat completion refused by the upstream content filter ({})",
            self.detail
        )
    }
}

impl std::error::Error for ContentFiltered {}

/// #144: whether `error`, or one of its causes, is a [`ContentFiltered`].
pub fn content_filtered(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ContentFiltered>())
}

/// The first [`BODY_EXCERPT_CHARS`] characters of a refused call's body
/// (characters, never bytes: a multi-byte character is never split).
pub fn body_excerpt(body: &str) -> String {
    body.chars().take(BODY_EXCERPT_CHARS).collect()
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
