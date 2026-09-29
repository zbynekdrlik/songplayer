//! The Gemini API contract every Gemini caller shares (#136): the API root,
//! the `gemini_api_key` key LIST, and what a refused answer means for it.
//!
//! The setting `gemini_api_key` is a comma-separated list of keys. Every
//! caller splits it with [`gemini_keys_from_setting`] and sends ONE key per
//! request in the `x-goog-api-key` header — never the list, never a URL
//! parameter. A non-2xx answer is judged by [`key_verdict`] alone, and one
//! key's request (with its same-key 5xx retries) is sent by [`send_on_key`]
//! alone, so the lyrics transcription (`lyrics::g35t_client`) and the
//! metadata provider (`metadata::gemini`) rotate over the same list by the
//! same rules and the same loop. Before #136 they had two diverging copies,
//! and the metadata one sent the whole list as one key.
//!
//! Callers differ only in what they do with the list: lyrics and metadata
//! rotate over every key; the dub worker (`dabing::worker`) takes the first
//! key only.

use std::time::Duration;

/// Root of every Gemini API endpoint.
pub const GEMINI_API_ROOT: &str = "https://generativelanguage.googleapis.com";

/// Pauses before each retry of a 5xx on the SAME key: up to 4 retries (5
/// attempts) at 2 / 4 / 8 / 16 s.
pub const RETRY_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// Split the `gemini_api_key` setting into its keys: trimmed, empties
/// dropped, in order.
pub fn gemini_keys_from_setting(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// What a non-2xx answer means for the key that got it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyVerdict {
    /// This key cannot answer now — try the NEXT key. `rate_limited`: a 429
    /// (out of quota); otherwise a key refusal (a 403, or a 400 whose body
    /// names the API key: `API_KEY_INVALID`, "API key not valid", "API key
    /// expired").
    NextKey { rate_limited: bool },
    /// A 5xx: the service, not the key — retry the SAME key after the next
    /// [`RETRY_BACKOFFS`] pause; stop once they are used up.
    RetrySameKey,
    /// Anything else (a bad request, an unknown model): every key would fail
    /// the same way — stop.
    Stop,
}

/// One key's reply from [`send_on_key`].
#[derive(Debug)]
pub enum KeyReply {
    /// A 2xx answer.
    Answered(reqwest::Response),
    /// A non-2xx answer this key did not turn into a 2xx: its verdict (a
    /// `RetrySameKey` here means every pause was used up), status and body.
    Refused {
        verdict: KeyVerdict,
        status: u16,
        body: String,
    },
}

/// Send one key's request: `build()` once, then again after each of
/// `backoffs` while the answer is a 5xx (`KeyVerdict::RetrySameKey`); any
/// other answer is returned at once. A transport failure is returned as is
/// (it is not about the key). `build` makes a fresh request per attempt (a
/// `RequestBuilder` is consumed by `send`); the key is the caller's, set in
/// `build` — this never logs a header or a key.
pub async fn send_on_key(
    what: &str,
    backoffs: &[Duration],
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<KeyReply, reqwest::Error> {
    let mut pauses = backoffs.iter();
    loop {
        let resp = build().send().await?;
        let status = resp.status().as_u16();
        if resp.status().is_success() {
            return Ok(KeyReply::Answered(resp));
        }
        let body = resp.text().await.unwrap_or_default();
        let verdict = key_verdict(status, &body);
        if verdict == KeyVerdict::RetrySameKey
            && let Some(pause) = pauses.next()
        {
            tracing::warn!(
                what,
                status,
                pause_ms = pause.as_millis() as u64,
                retries_left = pauses.len(),
                "gemini: 5xx — retrying the same key"
            );
            tokio::time::sleep(*pause).await;
            continue;
        }
        return Ok(KeyReply::Refused {
            verdict,
            status,
            body,
        });
    }
}

/// The verdict on one key's non-2xx `status` + response `body`.
pub fn key_verdict(status: u16, body: &str) -> KeyVerdict {
    match status {
        429 => KeyVerdict::NextKey { rate_limited: true },
        403 => KeyVerdict::NextKey {
            rate_limited: false,
        },
        400 if body.contains("API_KEY") || body.contains("API key") => KeyVerdict::NextKey {
            rate_limited: false,
        },
        500..=599 => KeyVerdict::RetrySameKey,
        _ => KeyVerdict::Stop,
    }
}

#[cfg(test)]
#[path = "gemini_api_tests.rs"]
mod tests;
