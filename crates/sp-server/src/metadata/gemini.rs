//! Gemini AI metadata provider.

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::LazyLock;
use std::time::Duration;

use super::{MetadataError, MetadataProvider};

static JSON_FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```(?:json)?\s*([\s\S]*?)\s*```").expect("compile"));

static JSON_OBJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{[^{}]*\}").expect("compile"));

/// Google Gemini API metadata provider.
pub struct GeminiProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }

    /// Build the API endpoint URL.
    fn endpoint(&self) -> String {
        format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        )
    }

    /// Build the request body for the Gemini API.
    fn build_request_body(&self, video_id: &str, title: &str) -> Value {
        let prompt = format!(
            "Extract the song name and artist from this YouTube video.\n\
             \n\
             YouTube URL: https://www.youtube.com/watch?v={video_id}\n\
             Video title: {title}\n\
             \n\
             Use Google Search to find the correct song name and artist.\n\
             \n\
             Rules:\n\
             - Return ONLY a JSON object with \"song\" and \"artist\" keys\n\
             - Use the official song name (not the video title)\n\
             - Use the original/primary artist name\n\
             - Do not include feat./ft. artists in the artist field\n\
             - If you cannot determine the song/artist, use the video title as song \
               and \"Unknown Artist\" as artist\n\
             \n\
             Examples:\n\
             {{\"song\": \"Bohemian Rhapsody\", \"artist\": \"Queen\"}}\n\
             {{\"song\": \"The Blessing\", \"artist\": \"Elevation Worship\"}}"
        );

        serde_json::json!({
            "system_instruction": {
                "parts": [{"text": "You are a JSON API. Return only valid JSON, no markdown, no explanation."}]
            },
            "contents": [
                {"role": "user", "parts": [{"text": prompt}]}
            ],
            "tools": [{"google_search": {}}],
            "generationConfig": {
                "temperature": 0.1
            }
        })
    }

    /// Parse a Gemini API response into `VideoMetadata`.
    fn parse_response(text: &str) -> Result<VideoMetadata, MetadataError> {
        let json_str = extract_json(text)?;

        let parsed: Value = serde_json::from_str(&json_str)
            .map_err(|e| MetadataError::InvalidResponse(format!("JSON parse error: {e}")))?;

        let song = parsed
            .get("song")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'song' field".into()))?;

        let artist = parsed
            .get("artist")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'artist' field".into()))?;

        Ok(VideoMetadata {
            song,
            artist,
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }
}

/// Extract JSON from a response that may contain markdown fences or mixed text.
fn extract_json(text: &str) -> Result<String, MetadataError> {
    let trimmed = text.trim();

    // Try direct parse first
    if trimmed.starts_with('{') && serde_json::from_str::<Value>(trimmed).is_ok() {
        return Ok(trimmed.to_string());
    }

    // Try markdown fence extraction
    if let Some(caps) = JSON_FENCE_RE.captures(trimmed) {
        let inner = caps[1].trim();
        if serde_json::from_str::<Value>(inner).is_ok() {
            return Ok(inner.to_string());
        }
    }

    // Regex fallback: find any JSON object in the text
    for m in JSON_OBJECT_RE.find_iter(trimmed) {
        if serde_json::from_str::<Value>(m.as_str()).is_ok() {
            return Ok(m.as_str().to_string());
        }
    }

    Err(MetadataError::InvalidResponse(
        "no valid JSON found in response".into(),
    ))
}

#[async_trait]
impl MetadataProvider for GeminiProvider {
    async fn extract(&self, video_id: &str, title: &str) -> Result<VideoMetadata, MetadataError> {
        let body = self.build_request_body(video_id, title);
        let url = self.endpoint();

        let mut last_err = MetadataError::ApiError("no attempts made".into());

        // Up to 3 attempts (initial + 2 retries on 429)
        for attempt in 0..3 {
            if attempt > 0 {
                let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32));
                tokio::time::sleep(delay).await;
            }

            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| MetadataError::ApiError(e.to_string()))?;

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                last_err = MetadataError::RateLimited;
                continue;
            }

            if !resp.status().is_success() {
                return Err(MetadataError::ApiError(format!("HTTP {}", resp.status())));
            }

            let response_body: Value = resp
                .json()
                .await
                .map_err(|e| MetadataError::InvalidResponse(e.to_string()))?;

            let text = response_body
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    MetadataError::InvalidResponse(
                        "missing candidates[0].content.parts[0].text".into(),
                    )
                })?;

            return Self::parse_response(text);
        }

        Err(last_err)
    }

    fn name(&self) -> &str {
        "gemini"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_direct() {
        let input = r#"{"song": "Test", "artist": "Artist"}"#;
        assert_eq!(extract_json(input).unwrap(), input);
    }

    #[test]
    fn extract_json_markdown_fence() {
        let input = "```json\n{\"song\": \"Test\", \"artist\": \"Artist\"}\n```";
        let result = extract_json(input).unwrap();
        assert_eq!(result, r#"{"song": "Test", "artist": "Artist"}"#);
    }

    #[test]
    fn extract_json_fence_without_lang() {
        let input = "```\n{\"song\": \"Test\", \"artist\": \"Artist\"}\n```";
        let result = extract_json(input).unwrap();
        assert_eq!(result, r#"{"song": "Test", "artist": "Artist"}"#);
    }

    #[test]
    fn extract_json_mixed_text() {
        let input =
            "Here is the result: {\"song\": \"Test\", \"artist\": \"Artist\"} hope this helps!";
        let result = extract_json(input).unwrap();
        assert_eq!(result, r#"{"song": "Test", "artist": "Artist"}"#);
    }

    #[test]
    fn extract_json_no_json() {
        assert!(extract_json("no json here").is_err());
    }

    #[test]
    fn parse_response_valid() {
        let text = r#"{"song": "Bohemian Rhapsody", "artist": "Queen"}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.song, "Bohemian Rhapsody");
        assert_eq!(meta.artist, "Queen");
        assert_eq!(meta.source, MetadataSource::Gemini);
        assert!(!meta.gemini_failed);
    }

    #[test]
    fn parse_response_with_fences() {
        let text = "```json\n{\"song\": \"The Blessing\", \"artist\": \"Elevation Worship\"}\n```";
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.song, "The Blessing");
        assert_eq!(meta.artist, "Elevation Worship");
    }

    #[test]
    fn parse_response_missing_song() {
        let text = r#"{"artist": "Queen"}"#;
        assert!(GeminiProvider::parse_response(text).is_err());
    }

    #[test]
    fn parse_response_missing_artist() {
        let text = r#"{"song": "Test"}"#;
        assert!(GeminiProvider::parse_response(text).is_err());
    }

    #[test]
    fn parse_response_empty_song() {
        let text = r#"{"song": "", "artist": "Queen"}"#;
        assert!(GeminiProvider::parse_response(text).is_err());
    }

    #[test]
    fn parse_response_trims_whitespace() {
        let text = r#"{"song": "  Test  ", "artist": "  Artist  "}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.song, "Test");
        assert_eq!(meta.artist, "Artist");
    }

    // ---- Async tests for provider chain (mock-based) ----

    #[tokio::test]
    async fn provider_constructs_and_names() {
        // The retry logic calls the real Gemini endpoint which we cannot
        // mock without DI for the URL. We verify the critical parse/extract
        // paths via the unit tests above. A full retry integration test would
        // need a test HTTP server with the URL injected into GeminiProvider.
        //
        // Verify that the provider at least constructs correctly:
        let _provider = GeminiProvider::new("test-key".into(), "test-model".into());
        assert_eq!(_provider.name(), "gemini");
    }
}
