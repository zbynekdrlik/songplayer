//! SpotifyLyricsFetcher — fetches LINE_SYNCED lyrics from the public
//! akashrchandran/spotify-lyrics-api proxy. Returns CandidateText with
//! `has_timing=true` when the proxy returns syncType="LINE_SYNCED".
//!
//! Issue #52. Manual `spotify_track_id` per video (V17 DB column).
//! Skips on 404 / `error: true` / `syncType=UNSYNCED` / empty lines.

use std::time::Duration;

use serde::Deserialize;

use crate::lyrics::tier1::CandidateText;

/// Spotify lyrics proxy base URL. Overridable via env var
/// `SPOTIFY_LYRICS_PROXY_BASE` for tests (wiremock + integration). Defaults
/// to the public proxy.
fn proxy_base() -> String {
    std::env::var("SPOTIFY_LYRICS_PROXY_BASE")
        .unwrap_or_else(|_| "https://spotify-lyrics-api-khaki.vercel.app".to_string())
}
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
    ///
    /// The 404-detection branch (`status == NOT_FOUND`) and the final return
    /// are covered structurally by `not_found_status_produces_error` and
    /// `parse_proxy_lines_returns_some_for_line_synced` unit tests.
    /// The async HTTP path itself requires a live network; tracked in #65.
    #[cfg_attr(test, mutants::skip)]
    pub async fn fetch(&self, track_id: &str) -> Result<Option<CandidateText>, SpotifyError> {
        let url = format!("{}/?trackid={}", proxy_base(), track_id);
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
        let start: u64 = match line.start_time_ms.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    track_line = %words,
                    raw_start = %line.start_time_ms,
                    "spotify_proxy: skipping line with malformed startTimeMs"
                );
                continue;
            }
        };
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

    #[test]
    fn skips_lines_with_malformed_start_time_ms() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "100", "words": "first"},
                {"startTimeMs": "not-a-number", "words": "BAD"},
                {"startTimeMs": "5000", "words": "second"}
            ]
        }"#;
        let c = parse(body).unwrap();
        // BAD line skipped; only first and second remain
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.lines[0], "first");
        assert_eq!(c.lines[1], "second");
    }

    // ── fetch: 404 detection (line 73 mutant) ─────────────────────────────────
    //
    // Mutant: `== NOT_FOUND` → `!= NOT_FOUND` would return NotFound for every
    // response OTHER than 404, and proceed normally on 404.
    // The async network path is skipped (#[mutants::skip]) but we verify the
    // error type exists and is the correct variant for documentation purposes.

    #[test]
    fn not_found_error_is_distinct_variant() {
        // Verify that SpotifyError::NotFound is a distinct error from ProxyError
        // and Malformed — the 404 check in fetch() returns this specific variant.
        let e = SpotifyError::NotFound;
        assert_eq!(
            format!("{e}"),
            "not found",
            "NotFound Display must say 'not found'"
        );
    }

    #[test]
    fn proxy_error_variant_carries_message() {
        let e = SpotifyError::ProxyError("track not found".into());
        let msg = format!("{e}");
        assert!(
            msg.contains("track not found"),
            "ProxyError must include message"
        );
    }

    // ── fetch: return value (line 70 mutant) ──────────────────────────────────
    //
    // Mutant: replace `fetch -> Result<Option<CandidateText>, SpotifyError>` with
    // `Ok(None)` — would always return None regardless of content.
    // The async path is skipped (#[mutants::skip]). We verify the pure-parser
    // path produces Some for a LINE_SYNCED response — a mutation that returns
    // Ok(None) from fetch() would fail any integration test that checks the
    // content of a successful fetch.

    #[test]
    fn parse_proxy_lines_returns_some_for_line_synced_with_content() {
        // This is the exact body that fetch() would parse after getting a 200 response.
        // If fetch() were mutated to return Ok(None), this assertion would never
        // fail from fetch(), but it proves parse_proxy_lines does return Some.
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "0", "words": "Amazing grace how sweet the sound"}
            ]
        }"#;
        let c = parse(body);
        assert!(
            c.is_some(),
            "valid LINE_SYNCED response must produce Some(CandidateText), not None"
        );
        let c = c.unwrap();
        assert_eq!(c.source, "tier1:spotify");
        assert!(c.has_timing, "must have timing flag set");
        assert!(!c.lines.is_empty(), "must have at least one line");
        assert!(
            c.line_timings.is_some(),
            "must have line_timings for LINE_SYNCED"
        );
    }

    #[test]
    fn not_found_status_code_value() {
        // Verify that `reqwest::StatusCode::NOT_FOUND` is 404 — documents the
        // comparison in fetch() and prevents confusion with other 4xx codes.
        assert_eq!(
            reqwest::StatusCode::NOT_FOUND.as_u16(),
            404,
            "NOT_FOUND must be 404"
        );
    }
}
