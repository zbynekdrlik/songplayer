//! Shared fixtures for the metadata-chain tests (#136): the fixed real video
//! the post-deploy gate probes, and mock-server answers in the exact shapes
//! OpenRouter (OpenAI chat) and Gemini (`generateContent`) return.

use std::sync::Arc;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::ai::client::AiClient;

/// The video of the #136 report (playlist ytfast, video 258).
pub const VIDEO: &str = "gq-4FVRr_ow";
/// Its YouTube title, verbatim (YouTube oEmbed) — the whole string the title
/// parser stored as `song`.
pub const TITLE: &str =
    "Stand On Your Promise by The Emerging Sound (feat. Maddie Fong & Brenton Lawless)";
pub const SONG: &str = "Stand On Your Promise";
pub const ARTIST: &str = "The Emerging Sound";

/// A `generateContent` answer whose text is `{"song", "artist"}` JSON.
pub fn gemini_answer(song: &str, artist: &str) -> Value {
    let text = json!({"song": song, "artist": artist}).to_string();
    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}]})
}

/// An `AiClient` on `server` (OpenRouter's OpenAI-compatible root, `/v1`).
pub async fn ai_client_at(server: &MockServer) -> Arc<AiClient> {
    Arc::new(crate::ai::test_support::ai_client_at(format!("{}/v1", server.uri())).await)
}

/// The OpenRouter text provider answers every metadata prompt with `song` /
/// `artist`.
pub async fn openrouter_answers(server: &MockServer, song: &str, artist: &str) {
    let content = json!({"song": song, "artist": artist}).to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices": [{"message": {"content": content}}]})),
        )
        .mount(server)
        .await;
}

/// OpenRouter refuses every prompt with a 400 (not retried by `AiClient`,
/// so the chain moves on at once).
pub async fn openrouter_refuses(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("unknown provider for model"))
        .mount(server)
        .await;
}

/// The `x-goog-api-key` header of every request `server` received, in order.
pub async fn received_keys(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            r.headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string()
        })
        .collect()
}

/// The body Google answers a refused key with (400 `API_KEY_INVALID`).
pub fn key_invalid_body() -> Value {
    json!({"error": {
        "code": 400,
        "message": "API key not valid. Please pass a valid API key.",
        "status": "INVALID_ARGUMENT",
        "details": [{"reason": "API_KEY_INVALID", "domain": "googleapis.com"}]
    }})
}
