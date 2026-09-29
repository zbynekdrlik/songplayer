//! Gemini AI metadata provider.
//!
//! #136: the setting `gemini_api_key` is a comma-separated key LIST (the
//! lyrics and dabing paths rotate over it). The provider takes the split
//! list (`chain::provider_chain` splits it with
//! `gemini_api::gemini_keys_from_setting`) and sends ONE key per attempt in
//! the `x-goog-api-key` header. Before #136 it sent the whole CSV as one key,
//! Google refused every call in ~40 ms, and every new video fell back to the
//! title parser.
//!
//! Rotation (`GeminiProvider::post_rotating`) follows the shared Gemini
//! key-list contract (`crate::gemini_api::key_verdict`, the same rules as
//! the lyrics transcription): a 429 or a key refusal moves to the NEXT key;
//! a 5xx retries the SAME key after each `gemini_api::RETRY_BACKOFFS` pause;
//! anything else stops at once, since the same request fails the same way on
//! every key. When every key failed, the error names the last key's INDEX,
//! its HTTP status and a short body excerpt with every configured key
//! redacted — never a key. It is [`MetadataError::RateLimited`] when any key
//! was rate-limited (the reprocess worker then cools down), else
//! [`MetadataError::ApiError`]. Logs carry the key index, the status and the
//! latency, never a key or a header.

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use super::parser::shorten_artist;
use super::sanitize::strip_emoji;
use super::{MetadataError, MetadataProvider};
use crate::gemini_api::{GEMINI_API_ROOT, KeyReply, KeyVerdict, RETRY_BACKOFFS, send_on_key};

static JSON_FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```(?:json)?\s*([\s\S]*?)\s*```").expect("compile"));

static JSON_OBJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{[^{}]*\}").expect("compile"));

/// Bound of one request. A grounded answer takes seconds; without a bound a
/// stalled connection would hang the download worker and the probe route.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Characters of an error body kept in an error text / log line.
const BODY_EXCERPT_CHARS: usize = 200;

/// Google Gemini API metadata provider.
pub struct GeminiProvider {
    /// The `gemini_api_key` list, tried in order. Never logged.
    keys: Vec<String>,
    model: String,
    /// `https://generativelanguage.googleapis.com` in production; a mock
    /// server's URI in tests.
    api_root: String,
    /// Pauses before each same-key retry of a 5xx: `gemini_api::RETRY_BACKOFFS`
    /// (milliseconds in tests).
    retry_backoffs: Vec<Duration>,
    client: reqwest::Client,
}

/// One key's outcome in `post_rotating`.
enum KeyAttempt {
    /// A 2xx answer.
    Answered(Value),
    /// `KeyVerdict::NextKey`: `last` describes the refusal (index, status,
    /// redacted excerpt).
    NextKey { rate_limited: bool, last: String },
    /// `Stop`, or a 5xx after every retry: every key would fail the same way.
    Stop(String),
}

impl GeminiProvider {
    /// A provider on Google's API root. `keys` is the split `gemini_api_key`
    /// list (one key per attempt).
    pub fn new(keys: Vec<String>, model: String) -> Self {
        Self::with_api_root(keys, model, GEMINI_API_ROOT)
    }

    /// [`GeminiProvider::new`] with the API root injected (a mock server in
    /// tests; `chain::provider_chain_at`).
    pub fn with_api_root(keys: Vec<String>, model: String, api_root: &str) -> Self {
        Self {
            keys,
            model,
            api_root: api_root.trim_end_matches('/').to_string(),
            retry_backoffs: RETRY_BACKOFFS.to_vec(),
            client: reqwest::Client::new(),
        }
    }

    /// Tests: short same-key retry pauses instead of 2 / 4 / 8 / 16 s.
    #[cfg(test)]
    fn with_retry_backoffs(mut self, backoffs: Vec<Duration>) -> Self {
        self.retry_backoffs = backoffs;
        self
    }

    /// The `generateContent` endpoint. The key goes in the `x-goog-api-key`
    /// header, never in the URL.
    fn endpoint(&self) -> String {
        format!(
            "{}/v1beta/models/{}:generateContent",
            self.api_root, self.model
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
    #[cfg_attr(test, mutants::skip)]
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

    /// POST `body` with one key per attempt until a key gets a 2xx answer
    /// (module doc: which statuses move to the next key). Returns the index
    /// of the key that answered and the answer's JSON.
    async fn post_rotating(
        &self,
        what: &str,
        body: &Value,
    ) -> Result<(usize, Value), MetadataError> {
        if self.keys.is_empty() {
            return Err(MetadataError::ApiError(format!(
                "gemini {what}: no API key configured (setting gemini_api_key is empty)"
            )));
        }
        let total = self.keys.len();
        let mut any_rate_limited = false;
        let mut last = String::new();
        for (index, key) in self.keys.iter().enumerate() {
            let n = index + 1;
            match self.attempt_key(what, body, key, n, total).await? {
                KeyAttempt::Answered(answer) => return Ok((index, answer)),
                KeyAttempt::NextKey {
                    rate_limited,
                    last: refusal,
                } => {
                    any_rate_limited |= rate_limited;
                    last = refusal;
                }
                KeyAttempt::Stop(reason) => {
                    return Err(MetadataError::ApiError(format!("gemini {what}: {reason}")));
                }
            }
        }
        let detail = format!("gemini {what}: all {total} keys failed; last {last}");
        Err(if any_rate_limited {
            MetadataError::RateLimited(detail)
        } else {
            MetadataError::ApiError(detail)
        })
    }

    /// One key (`n` of `total`) through the shared `gemini_api::send_on_key`
    /// (a 5xx retried on the same key after each `retry_backoffs` pause). A
    /// transport failure is not about the key: it ends the whole call.
    async fn attempt_key(
        &self,
        what: &str,
        body: &Value,
        key: &str,
        n: usize,
        total: usize,
    ) -> Result<KeyAttempt, MetadataError> {
        let started = Instant::now();
        let reply = send_on_key(what, &self.retry_backoffs, || {
            self.client
                .post(self.endpoint())
                .header("x-goog-api-key", key)
                .timeout(REQUEST_TIMEOUT)
                .json(body)
        })
        .await
        .map_err(|e| {
            MetadataError::ApiError(format!(
                "gemini {what}: key {n} of {total}: request failed: {}",
                self.excerpt(&e.to_string())
            ))
        })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (verdict, status, text) = match reply {
            KeyReply::Answered(resp) => {
                let status = resp.status().as_u16();
                tracing::info!(
                    what,
                    key = n,
                    keys = total,
                    status,
                    elapsed_ms,
                    "gemini answered"
                );
                let answer = resp
                    .json::<Value>()
                    .await
                    .map_err(|e| MetadataError::InvalidResponse(format!("gemini {what}: {e}")))?;
                return Ok(KeyAttempt::Answered(answer));
            }
            KeyReply::Refused {
                verdict,
                status,
                body,
            } => (verdict, status, body),
        };
        let excerpt = self.excerpt(&text);
        tracing::warn!(
            what, key = n, keys = total, status, elapsed_ms, ?verdict, body = %excerpt,
            "gemini refused the request"
        );
        let last = format!("key {n} of {total}: HTTP {status}: {excerpt}");
        Ok(match verdict {
            KeyVerdict::NextKey { rate_limited } => KeyAttempt::NextKey { rate_limited, last },
            // A 5xx after every same-key pause, or anything else: every key
            // would fail the same way.
            KeyVerdict::RetrySameKey | KeyVerdict::Stop => KeyAttempt::Stop(last),
        })
    }

    /// Second pass: clean the extracted song/artist for display, on the key
    /// that answered the first pass. Any failure keeps the first pass's
    /// values (the clean-up is cosmetic, the first answer already names the
    /// song).
    async fn clean_for_display(
        &self,
        key_index: usize,
        song: &str,
        artist: &str,
    ) -> (String, String) {
        let uncleaned = (song.to_string(), artist.to_string());
        let body = self.build_clean_body(song, artist);
        let resp = self
            .client
            .post(self.endpoint())
            .header("x-goog-api-key", &self.keys[key_index])
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(
                    status = r.status().as_u16(),
                    "clean_for_display refused, keeping uncleaned"
                );
                return uncleaned;
            }
            Err(e) => {
                tracing::debug!(error = %self.excerpt(&e.to_string()), "clean_for_display failed, keeping uncleaned");
                return uncleaned;
            }
        };
        let Ok(answer) = resp.json::<Value>().await else {
            return uncleaned;
        };
        let Some(parsed) = response_text(&answer)
            .and_then(|text| extract_json(&text).ok())
            .and_then(|json| serde_json::from_str::<Value>(&json).ok())
        else {
            return uncleaned;
        };
        let clean_song = parsed
            .get("song")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(uncleaned.0);
        let clean_artist = parsed
            .get("artist")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or(uncleaned.1);
        (clean_song, clean_artist)
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

    /// `text` with every configured key replaced by `<key>`: an error text
    /// or a log line must never carry a key, whatever the server echoes.
    /// Longest key first, so a key that contains another one is replaced
    /// whole.
    fn redact(&self, text: &str) -> String {
        let mut keys: Vec<&str> = self
            .keys
            .iter()
            .map(String::as_str)
            .filter(|k| !k.is_empty())
            .collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        keys.into_iter()
            .fold(text.to_string(), |acc, k| acc.replace(k, "<key>"))
    }

    /// `body` on one line, redacted, cut to [`BODY_EXCERPT_CHARS`] characters
    /// (redacted BEFORE the cut, so no key prefix survives at the edge).
    fn excerpt(&self, body: &str) -> String {
        let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
        self.redact(&one_line)
            .chars()
            .take(BODY_EXCERPT_CHARS)
            .collect()
    }
}

/// The model's answer text: every text part of the first candidate that is
/// not a thought, joined. A grounded answer may arrive split over several
/// parts; `None` when there is no text at all.
fn response_text(answer: &Value) -> Option<String> {
    let parts = answer.pointer("/candidates/0/content/parts")?.as_array()?;
    let text: String = parts
        .iter()
        .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect();
    (!text.trim().is_empty()).then_some(text)
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
        let (key_index, answer) = self.post_rotating("search", &body).await?;
        let text = response_text(&answer).ok_or_else(|| {
            MetadataError::InvalidResponse(
                "gemini search: no text in candidates[0].content.parts".into(),
            )
        })?;
        let mut meta = Self::parse_response(&text)?;
        // Second pass: clean for display (strip collabs, subtitles).
        let (song, artist) = self
            .clean_for_display(key_index, &meta.song, &meta.artist)
            .await;
        meta.song = strip_emoji(&song);
        meta.artist = strip_emoji(&artist);
        Ok(meta)
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
        let provider = GeminiProvider::new(vec!["test-key".into()], "gemini-2.5-flash".into());
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
        let provider = GeminiProvider::new(vec!["test-key".into()], "gemini-2.5-flash".into());
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

    /// Boundary test: char at exactly U+25FF (just below 0x2600) must be kept.
    /// Kills the `< 0x2600` → `<= 0x2600` mutant.
    #[test]
    fn strip_emoji_keeps_chars_below_symbol_range() {
        // U+25A0 = ■ (geometric shape, below 0x2600) — should be kept
        let s = "Test\u{25A0}End";
        assert_eq!(strip_emoji(s), "Test\u{25A0}End");
    }

    /// Char at U+2600 (☀ sun) should be stripped.
    /// Kills the `< 0x2600` → `<= 0x2600` and `||` → `&&` mutants.
    #[test]
    fn strip_emoji_removes_symbol_at_boundary() {
        assert_eq!(strip_emoji("Sun \u{2600} Rise"), "Sun Rise");
    }

    // ---- Async tests for provider chain (mock-based) ----

    #[tokio::test]
    async fn provider_constructs_and_names() {
        // The key rotation + HTTP paths run against a mock server in
        // `gemini_tests_keys.rs` (#136: `GeminiProvider::with_api_root`).
        let provider = GeminiProvider::new(vec!["test-key".into()], "test-model".into());
        assert_eq!(provider.name(), "gemini");
    }
}

#[cfg(test)]
#[path = "gemini_tests_keys.rs"]
mod tests_keys;
