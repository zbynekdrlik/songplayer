//! Gemini AI metadata provider.

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::LazyLock;
use std::time::Duration;

use super::parser::shorten_artist;
use super::{MetadataError, MetadataProvider};

/// Replace common emojis with text equivalents, then strip remaining non-text chars.
fn strip_emoji(s: &str) -> String {
    // Replace known emojis with text first (before stripping)
    let replaced = s
        .replace('\u{2764}', "Love") // ❤ red heart
        .replace('\u{1F90D}', "Love") // 🤍 white heart
        .replace('\u{1F499}', "Love") // 💙 blue heart
        .replace('\u{1F49C}', "Love") // 💜 purple heart
        .replace('\u{2665}', "Love") // ♥ heart suit
        .replace('\u{1F525}', "") // 🔥 fire
        .replace('\u{1F64F}', "") // 🙏 pray
        .replace('\u{2728}', "") // ✨ sparkles
        .replace('\u{1F3B6}', "") // 🎶 notes
        .replace('\u{1F3B5}', ""); // 🎵 note
    // Strip any remaining non-text characters
    replaced
        .chars()
        .filter(|c| {
            let cp = *c as u32;
            cp < 0x2600 || (0xFE00..=0xFE0F).contains(&cp) || (0x00C0..=0x024F).contains(&cp)
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

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
        let video_url = format!("https://www.youtube.com/watch?v={video_id}");
        let prompt = format!(
            "Look up information about this YouTube video and extract the artist and song title:\n\
             URL: {video_url}\n\
             Title: \"{title}\"\n\
             \n\
             Use Google Search to find information about this specific YouTube video URL.\n\
             \n\
             CRITICAL: Respond with ONLY a valid JSON object. No explanatory text allowed.\n\
             \n\
             Return EXACTLY this format:\n\
             {{\"artist\": \"Primary Artist Name\", \"song\": \"Song Title\"}}\n\
             \n\
             IMPORTANT RULES:\n\
             1. Search for the YouTube URL to find the actual artist and song information\n\
             2. For worship/church music, identify the performing artist/band (not the church name)\n\
             3. Return ONLY the primary artist/band. Remove ALL featured/secondary artists, collaborators, \
                and \"feat./ft./featuring\" credits. \"Elevation Worship & Chandler Moore\" → just \"Elevation Worship\". \
                \"Maverick City Music x UPPERROOM\" → just \"Maverick City Music\".\n\
             4. Remove (Official Video), (Live), etc from song titles. Also remove parenthetical subtitles \
                like \"(We Crown You)\", \"(Moment)\", \"(Here In Your Presence)\" — return only the main song name.\n\
             5. For single songs with \"/\" in their actual title (like \"Faithful Then / Faithful Now\"), keep the full title\n\
             6. NEVER include album names in the song title - return only the actual song name\n\
             7. If the video is a medley or contains multiple distinct songs, return ONLY the first song\n\
             8. If no artist found, return empty string for artist. NEVER return \"Unknown Artist\".\n\
             9. NEVER fabricate or guess information. Only return data you found via search or can clearly extract from the title. \
                If the video is not a song (e.g. vocal workout, instrumental), return the title as song and empty artist.\n\
             10. For COVERS: use the performing artist from THIS video, NOT the original song's artist. \
                 If the title says \"(Cover) | New Heights Worship\", the artist is \"New Heights Worship\".\n\
             11. Preserve the artist's official brand casing. If an artist styles themselves in lowercase \
                 (like \"planetboom\", \"deadmau5\") or uppercase (like \"TAYA\"), keep that exact casing.\n\
             12. NEVER include emojis in song or artist. Replace emojis with their text meaning: \
                 heart emoji → \"Love\", fire emoji → remove, etc. Example: \"Yahweh We 🤍 You\" → \"Yahweh We Love You\".\n\
             \n\
             ARTIST NAME SHORTENING — apply these rules:\n\
             - For PERSONAL names (individual people), shorten first/middle names to initials: \
               \"Chris Tomlin\" → \"C. Tomlin\", \"Pat Barrett\" → \"P. Barrett\"\n\
             - NEVER abbreviate band/group/duo names. These stay in full: \
               \"Elevation Worship\", \"Planetshakers\", \"Hillsong Young & Free\", \"Sons Of Sunday\", \
               \"One Voice\", \"VOUS Worship\", \"Maverick City Music\"\n\
             - Spanish/foreign duo names stay in full: \"Johan y Sofi\" (do NOT shorten to \"J. Y. Sofi\")\n\
             - When in doubt whether a name is a person or a group, do NOT shorten it\n\
             \n\
             Examples:\n\
             - \"HOLYGHOST | Sons Of Sunday\" → {{\"artist\": \"Sons Of Sunday\", \"song\": \"HOLYGHOST\"}}\n\
             - \"'COME RIGHT NOW' | Official Video\" → {{\"artist\": \"Planetshakers\", \"song\": \"COME RIGHT NOW\"}}\n\
             - \"Supernatural Love | Show Me Your Glory - Live At Chapel | Planetshakers Official Music Video\" → {{\"artist\": \"Planetshakers\", \"song\": \"Supernatural Love\"}}\n\
             - \"Forever | Live At Chapel\" → {{\"artist\": \"K. Jobe\", \"song\": \"Forever\"}}\n\
             - \"The Blessing (Live) | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"The Blessing\"}}\n\
             - \"Faithful Then / Faithful Now | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"Faithful Then / Faithful Now\"}}\n\
             - \"There Is A King/What Would You Do | Live | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"There Is A King\"}}\n\
             - \"Pat Barrett - Count On You (Live)\" → {{\"artist\": \"P. Barrett\", \"song\": \"Count On You\"}}\n\
             - \"JIREH (Cover) | New Heights Worship\" → {{\"artist\": \"New Heights Worship\", \"song\": \"Jireh\"}}\n\
             - \"Puro - Johan y Sofi (Mantenme Puro)\" → {{\"artist\": \"Johan y Sofi\", \"song\": \"Puro\"}}\n\
             - \"Song For His Presence - Hillsong Young & Free\" → {{\"artist\": \"Hillsong Young & Free\", \"song\": \"Song For His Presence\"}}\n\
             - \"God I'm Just Grateful | Elevation Worship & Chandler Moore\" → {{\"artist\": \"Elevation Worship\", \"song\": \"God I'm Just Grateful\"}}\n\
             - \"No One Like The Lord (We Crown You) - Circuit Rider Music\" → {{\"artist\": \"Circuit Rider Music\", \"song\": \"No One Like The Lord\"}}\n\
             - \"Home (Here In Your Presence) | planetboom\" → {{\"artist\": \"planetboom\", \"song\": \"Home\"}}\n\
             \n\
             REMEMBER: Return ONLY valid JSON, nothing else. The song field should contain ONLY the song title, never album names or other metadata."
        );

        serde_json::json!({
            "system_instruction": {
                "parts": [{"text": "You are a JSON API that returns only valid JSON objects. Never include explanatory text, reasoning, or any content outside the JSON structure."}]
            },
            "contents": [
                {"role": "user", "parts": [{"text": prompt}]}
            ],
            "tools": [{"google_search": {}}],
            "generationConfig": {
                "temperature": 0.1,
                "candidateCount": 1
            }
        })
    }

    /// Build the second-pass cleaning prompt. No search needed — just formatting.
    fn build_clean_body(&self, song: &str, artist: &str) -> Value {
        let prompt = format!(
            "I have a song title and artist extracted from YouTube. Clean them for LED wall display.\n\
             \n\
             Song: \"{song}\"\n\
             Artist: \"{artist}\"\n\
             \n\
             CLEANING RULES:\n\
             1. ARTIST: Return ONLY the primary/main artist or band. Remove ALL secondary artists \
                after \"&\", \"x\", \"feat.\", \"ft.\", \",\", \"and\", \"con\", \"with\". Examples:\n\
                - \"Elevation Worship & Chandler Moore\" → \"Elevation Worship\"\n\
                - \"Maverick City Music x UPPERROOM\" → \"Maverick City Music\"\n\
                - \"CityHill Worship & M. Sergeev\" → \"CityHill Worship\"\n\
                - \"SEU Worship, R. Stewart, G. Shuffitt\" → \"SEU Worship\"\n\
                - Single artists stay as-is: \"Planetshakers\", \"P. Barrett\", \"TAYA\"\n\
                - If one name is a well-known worship label/ministry (Bethel Music, Hillsong, Maverick City Music) \
                  and the other is an individual person, prefer the label/ministry as the primary artist.\n\
             2. SONG: Remove parenthetical subtitles/descriptions. Keep only the main song name:\n\
                - \"No One Like The Lord (We Crown You)\" → \"No One Like The Lord\"\n\
                - \"Home (Here In Your Presence)\" → \"Home\"\n\
                - But keep \"/\" medley titles: \"Faithful Then / Faithful Now\" stays\n\
             3. Remove language suffixes from artist names: \"Español\", \"Espanol\", \"Musica\" → drop them\n\
             4. Preserve original casing (lowercase \"planetboom\", uppercase \"TAYA\", etc.)\n\
             \n\
             Return JSON: {{\"song\": \"cleaned song\", \"artist\": \"cleaned artist\"}}"
        );

        serde_json::json!({
            "system_instruction": {
                "parts": [{"text": "You are a JSON API. Return only valid JSON."}]
            },
            "contents": [
                {"role": "user", "parts": [{"text": prompt}]}
            ],
            "generationConfig": {
                "temperature": 0.0,
                "candidateCount": 1
            }
        })
    }

    /// Second pass: clean the extracted song/artist for display.
    async fn clean_for_display(
        &self,
        song: &str,
        artist: &str,
    ) -> Result<(String, String), MetadataError> {
        let body = self.build_clean_body(song, artist);
        let url = self.endpoint();

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MetadataError::ApiError(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // If rate-limited on the cleaning pass, just return the uncleaned values
            tracing::debug!("clean_for_display rate-limited, returning uncleaned");
            return Ok((song.to_string(), artist.to_string()));
        }

        if !resp.status().is_success() {
            return Ok((song.to_string(), artist.to_string()));
        }

        let response_body: Value = resp
            .json()
            .await
            .map_err(|e| MetadataError::InvalidResponse(e.to_string()))?;

        let text = response_body
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match extract_json(text) {
            Ok(json_str) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&json_str) {
                    let clean_song = parsed
                        .get("song")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| song.to_string());
                    let clean_artist = parsed
                        .get("artist")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| artist.to_string());
                    return Ok((clean_song, clean_artist));
                }
            }
            Err(_) => {}
        }

        Ok((song.to_string(), artist.to_string()))
    }

    /// Parse a Gemini API response into `VideoMetadata`.
    fn parse_response(text: &str) -> Result<VideoMetadata, MetadataError> {
        let json_str = extract_json(text)?;

        let parsed: Value = serde_json::from_str(&json_str)
            .map_err(|e| MetadataError::InvalidResponse(format!("JSON parse error: {e}")))?;

        let song = parsed
            .get("song")
            .and_then(|v| v.as_str())
            .map(|s| strip_emoji(s.trim()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'song' field".into()))?;

        let artist_raw = parsed
            .get("artist")
            .and_then(|v| v.as_str())
            .map(|s| strip_emoji(s.trim()))
            .unwrap_or_default();

        let artist = if artist_raw.is_empty() {
            String::new()
        } else {
            shorten_artist(&artist_raw)
        };

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

            let mut meta = Self::parse_response(text)?;

            // Second pass: clean for display (strip collabs, subtitles)
            if !meta.song.is_empty() {
                match self.clean_for_display(&meta.song, &meta.artist).await {
                    Ok((clean_song, clean_artist)) => {
                        meta.song = clean_song;
                        meta.artist = clean_artist;
                    }
                    Err(e) => {
                        tracing::debug!("clean_for_display failed, keeping raw: {e}");
                    }
                }
            }

            return Ok(meta);
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
    fn parse_response_missing_artist_returns_empty() {
        let text = r#"{"song": "Test"}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.song, "Test");
        assert_eq!(meta.artist, "");
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

    #[test]
    fn build_request_body_contains_worship_rules() {
        let provider = GeminiProvider::new("test-key".into(), "gemini-2.5-flash".into());
        let body = provider.build_request_body("dQw4w9WgXcQ", "Test Title");
        let prompt = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert!(
            prompt.contains("worship"),
            "prompt must mention worship music"
        );
        assert!(prompt.contains("album"), "prompt must mention album names");
        assert!(prompt.contains("medley"), "prompt must mention medleys");
        assert!(
            prompt.contains("HOLYGHOST"),
            "prompt must have HOLYGHOST example"
        );
        assert!(
            prompt.contains("Planetshakers"),
            "prompt must have Planetshakers example"
        );
        assert!(
            prompt.contains("Faithful Then / Faithful Now"),
            "prompt must have slash example"
        );
        assert!(
            prompt.contains("shorten"),
            "prompt must mention artist shortening"
        );
        assert!(
            prompt.contains("COVERS"),
            "prompt must mention cover attribution"
        );
        assert!(
            prompt.contains("fabricate"),
            "prompt must warn against fabrication"
        );
        assert!(
            prompt.contains("Johan y Sofi"),
            "prompt must have Spanish duo example"
        );
    }

    #[test]
    fn build_request_body_has_google_search_tool() {
        let provider = GeminiProvider::new("test-key".into(), "gemini-2.5-flash".into());
        let body = provider.build_request_body("test", "Test");
        assert!(body["tools"][0]["google_search"].is_object());
    }

    #[test]
    fn parse_response_shortens_personal_artist() {
        let text = r#"{"song": "Count On You", "artist": "Pat Barrett"}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.artist, "P. Barrett");
    }

    #[test]
    fn parse_response_does_not_shorten_band() {
        let text = r#"{"song": "The Blessing", "artist": "Elevation Worship"}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.artist, "Elevation Worship");
    }

    #[test]
    fn parse_response_strips_emoji_from_song() {
        let text = r#"{"song": "Yahweh We 🤍 You", "artist": "Elevation Worship"}"#;
        let meta = GeminiProvider::parse_response(text).unwrap();
        assert_eq!(meta.song, "Yahweh We Love You");
        assert_eq!(meta.artist, "Elevation Worship");
    }

    #[test]
    fn strip_emoji_replaces_hearts_with_love() {
        assert_eq!(strip_emoji("Yahweh We 🤍 You"), "Yahweh We Love You");
        assert_eq!(strip_emoji("Song ❤ You"), "Song Love You");
    }

    #[test]
    fn strip_emoji_removes_non_heart_emoji() {
        assert_eq!(strip_emoji("Song 🔥 Title"), "Song Title");
        assert_eq!(strip_emoji("Normal Text"), "Normal Text");
        assert_eq!(strip_emoji("Café María"), "Café María");
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
