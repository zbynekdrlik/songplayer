//! SpotifyLyricsFetcher — fetches LINE_SYNCED lyrics from the public
//! akashrchandran/spotify-lyrics-api proxy. Returns CandidateText with
//! `has_timing=true` when the proxy returns syncType="LINE_SYNCED".
//!
//! Issue #52. Manual `spotify_track_id` per video (V17 DB column).
//! Skips on 404 / `error: true` / `syncType=UNSYNCED` / empty lines.

use std::time::Duration;

use serde::Deserialize;

use crate::lyrics::tier1::CandidateText;

const PROXY_BASE: &str = "https://spotify-lyrics-api-khaki.vercel.app";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum SpotifyError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("not found")]
    NotFound,
    #[error("proxy reported error: {0}")]
    ProxyError(String),
    #[error("malformed: {0}")]
    Malformed(String),
}

#[derive(Debug, Deserialize)]
struct ProxyResponse {
    error: Option<bool>,
    #[serde(rename = "syncType")]
    sync_type: Option<String>,
    lines: Option<Vec<ProxyLine>>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyLine {
    #[serde(rename = "startTimeMs")]
    start_time_ms: String,
    words: String,
}

pub struct SpotifyLyricsFetcher {
    http: reqwest::Client,
}

impl Default for SpotifyLyricsFetcher {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }
}

impl SpotifyLyricsFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch LINE_SYNCED lyrics for a Spotify track ID. Returns:
    /// - `Ok(Some(CandidateText))` if syncType == LINE_SYNCED with ≥1 non-filler line
    /// - `Ok(None)` if the track has no synced lyrics (UNSYNCED / empty)
    /// - `Err(SpotifyError)` on network / parse failure
    pub async fn fetch(&self, track_id: &str) -> Result<Option<CandidateText>, SpotifyError> {
        let url = format!("{PROXY_BASE}/?trackid={track_id}");
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(SpotifyError::NotFound);
        }
        let body = resp.text().await?;
        let parsed: ProxyResponse = serde_json::from_str(&body)
            .map_err(|e| SpotifyError::Malformed(format!("json: {e}")))?;
        if parsed.error.unwrap_or(false) {
            return Err(SpotifyError::ProxyError(
                parsed.message.unwrap_or_else(|| "proxy error".into()),
            ));
        }
        Ok(parse_proxy_lines(parsed))
    }
}

/// Pure parser broken out for unit testability — no network involved.
fn parse_proxy_lines(parsed: ProxyResponse) -> Option<CandidateText> {
    if parsed.sync_type.as_deref() != Some("LINE_SYNCED") {
        return None;
    }
    let raw_lines = parsed.lines.unwrap_or_default();
    if raw_lines.is_empty() {
        return None;
    }

    // Build CandidateText. Skip empty/♪ filler lines. End time = next line's start
    // (or last line + 3000ms).
    let mut texts = Vec::new();
    let mut timings = Vec::new();
    let n = raw_lines.len();
    for (i, line) in raw_lines.iter().enumerate() {
        let words = line.words.trim();
        if words.is_empty() || words == "♪" {
            continue;
        }
        let start: u64 = line.start_time_ms.parse().unwrap_or(0);
        let end: u64 = if i + 1 < n {
            raw_lines[i + 1]
                .start_time_ms
                .parse()
                .unwrap_or(start.saturating_add(3000))
        } else {
            start.saturating_add(3000)
        };
        texts.push(words.to_string());
        timings.push((start, end));
    }
    if texts.is_empty() {
        return None;
    }
    Some(CandidateText {
        source: "tier1:spotify".into(),
        lines: texts,
        line_timings: Some(timings),
        has_timing: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Option<CandidateText> {
        let parsed: ProxyResponse = serde_json::from_str(body).expect("fixture");
        if parsed.error.unwrap_or(false) {
            return None;
        }
        parse_proxy_lines(parsed)
    }

    #[test]
    fn parses_line_synced_response() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "1000", "words": "Hello world"},
                {"startTimeMs": "3000", "words": "Praise the Lord"}
            ]
        }"#;
        let c = parse(body).unwrap();
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.lines[0], "Hello world");
        assert_eq!(c.lines[1], "Praise the Lord");
        let timings = c.line_timings.as_ref().unwrap();
        assert_eq!(timings[0], (1000, 3000));
        assert_eq!(timings[1].1, 6000); // last line: start + 3000
        assert!(c.has_timing);
        assert_eq!(c.source, "tier1:spotify");
    }

    #[test]
    fn returns_none_for_unsynced_response() {
        let body = r#"{"error": false, "syncType": "UNSYNCED", "lines": []}"#;
        assert!(parse(body).is_none());
    }

    #[test]
    fn returns_none_for_word_synced_response() {
        // Spotify rarely emits WORD_SYNCED but if it ever does, this fetcher
        // does not handle it — only LINE_SYNCED.
        let body = r#"{"error": false, "syncType": "WORD_SYNCED", "lines": [{"startTimeMs": "0", "words": "x"}]}"#;
        assert!(parse(body).is_none());
    }

    #[test]
    fn returns_none_for_proxy_error() {
        let body = r#"{"error": true, "message": "track not found"}"#;
        assert!(parse(body).is_none());
    }

    #[test]
    fn skips_empty_filler_lines() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "1000", "words": "♪"},
                {"startTimeMs": "2000", "words": ""},
                {"startTimeMs": "2500", "words": "  "},
                {"startTimeMs": "3000", "words": "Real line"}
            ]
        }"#;
        let c = parse(body).unwrap();
        assert_eq!(c.lines.len(), 1);
        assert_eq!(c.lines[0], "Real line");
        assert_eq!(c.line_timings.as_ref().unwrap()[0], (3000, 6000));
    }

    #[test]
    fn returns_none_when_only_filler_lines() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "1000", "words": "♪"},
                {"startTimeMs": "2000", "words": "♪"}
            ]
        }"#;
        assert!(parse(body).is_none());
    }

    #[test]
    fn returns_none_for_empty_lines_array() {
        let body = r#"{"error": false, "syncType": "LINE_SYNCED", "lines": []}"#;
        assert!(parse(body).is_none());
    }

    #[test]
    fn end_time_for_intermediate_line_is_next_start() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "100", "words": "first"},
                {"startTimeMs": "5000", "words": "second"},
                {"startTimeMs": "9000", "words": "third"}
            ]
        }"#;
        let c = parse(body).unwrap();
        let timings = c.line_timings.as_ref().unwrap();
        assert_eq!(timings[0], (100, 5000));
        assert_eq!(timings[1], (5000, 9000));
        assert_eq!(timings[2], (9000, 12000)); // last: start + 3000ms
    }

    #[test]
    fn fetcher_constructs() {
        let _f = SpotifyLyricsFetcher::new();
        let _g = SpotifyLyricsFetcher::default();
    }
}
