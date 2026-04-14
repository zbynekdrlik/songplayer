//! LRCLIB API client and LRC format parser.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use tracing::debug;

#[cfg_attr(test, mutants::skip)]
fn user_agent() -> String {
    format!(
        "SongPlayer/{} (github.com/zbynekdrlik/songplayer)",
        env!("CARGO_PKG_VERSION")
    )
}
const LRCLIB_BASE: &str = "https://lrclib.net/api/get";
const REQUEST_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// LRCLIB API response shape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch lyrics from LRCLIB for the given artist, song, and duration.
///
/// Returns `None` when LRCLIB returns 404 (no match found).
/// Tries `syncedLyrics` first, then falls back to `plainLyrics`.
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_lyrics(
    client: &Client,
    artist: &str,
    song: &str,
    duration_s: u32,
) -> Result<Option<LyricsTrack>> {
    let url = format!(
        "{}?artist_name={}&track_name={}&duration={}",
        LRCLIB_BASE,
        urlencoding::encode(artist),
        urlencoding::encode(song),
        duration_s,
    );

    debug!("LRCLIB request: {}", url);

    let response = client
        .get(&url)
        .header("User-Agent", user_agent())
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        debug!("LRCLIB returned 404 for artist={} song={}", artist, song);
        return Ok(None);
    }

    let response = response.error_for_status()?;
    let body: LrclibResponse = response.json().await?;

    // Prefer synced lyrics (LRC format with timestamps)
    if let Some(lrc) = body.synced_lyrics.filter(|s| !s.trim().is_empty()) {
        debug!("LRCLIB: using synced lyrics");
        return Ok(parse_lrc(&lrc));
    }

    // Fall back to plain lyrics (no timestamps)
    if let Some(plain) = body.plain_lyrics.filter(|s| !s.trim().is_empty()) {
        debug!("LRCLIB: using plain lyrics fallback");
        return Ok(parse_plain(&plain));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// LRC parser
// ---------------------------------------------------------------------------

/// Parse an LRC-format string into a `LyricsTrack`.
///
/// LRC format: `[MM:SS.CC] Lyrics text`
///
/// - Each line's `end_ms` = next line's `start_ms`
/// - Last line gets `end_ms = start_ms + 5000`
/// - Lines with empty text are skipped
/// - Returns `None` if no valid lines were found
pub fn parse_lrc(lrc_text: &str) -> Option<LyricsTrack> {
    let mut lines: Vec<LyricsLine> = Vec::new();

    for raw in lrc_text.lines() {
        let raw = raw.trim();
        // Must start with '[' timestamp
        if !raw.starts_with('[') {
            continue;
        }

        // Find closing ']'
        let Some(close) = raw.find(']') else {
            continue;
        };
        let ts_str = &raw[1..close];
        let text = raw[close + 1..].trim().to_string();

        // Skip empty text lines
        if text.is_empty() {
            continue;
        }

        if let Some(start_ms) = parse_lrc_timestamp(ts_str) {
            lines.push(LyricsLine {
                start_ms,
                end_ms: 0, // filled in below
                en: text,
                sk: None,
                words: None,
            });
        }
    }

    if lines.is_empty() {
        return None;
    }

    // Fill end_ms: each line ends where the next begins
    let len = lines.len();
    for i in 0..len - 1 {
        lines[i].end_ms = lines[i + 1].start_ms;
    }
    // Last line: end = start + 5 seconds
    lines[len - 1].end_ms = lines[len - 1].start_ms + 5000;

    Some(LyricsTrack {
        version: 1,
        source: "lrclib".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines,
    })
}

/// Parse an LRC timestamp string `MM:SS.CC` (or variants) to milliseconds.
///
/// Handles 1, 2, or 3 fractional digit groups, and no fractional part.
#[cfg_attr(test, mutants::skip)]
pub fn parse_lrc_timestamp(ts: &str) -> Option<u64> {
    // Split on ':'
    let colon = ts.find(':')?;
    let minutes: u64 = ts[..colon].parse().ok()?;

    let rest = &ts[colon + 1..];

    let (seconds, millis) = if let Some(dot) = rest.find('.') {
        let secs: u64 = rest[..dot].parse().ok()?;
        let frac_str = &rest[dot + 1..];
        // Normalise to milliseconds regardless of number of fractional digits
        let millis: u64 = match frac_str.len() {
            0 => 0,
            1 => frac_str.parse::<u64>().ok()? * 100,
            2 => frac_str.parse::<u64>().ok()? * 10,
            3 => frac_str.parse::<u64>().ok()?,
            _ => {
                // More than 3 digits: take first 3
                frac_str[..3].parse::<u64>().ok()?
            }
        };
        (secs, millis)
    } else {
        let secs: u64 = rest.parse().ok()?;
        (secs, 0)
    };

    Some(minutes * 60 * 1000 + seconds * 1000 + millis)
}

// ---------------------------------------------------------------------------
// Plain lyrics parser
// ---------------------------------------------------------------------------

/// Parse plain (non-timestamped) lyrics into a `LyricsTrack`.
///
/// All lines get `start_ms = 0` and `end_ms = 0` — they will be aligned by a
/// separate alignment step in a later task.
///
/// Returns `None` if the text contains no non-empty lines.
pub fn parse_plain(text: &str) -> Option<LyricsTrack> {
    let lines: Vec<LyricsLine> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| LyricsLine {
            start_ms: 0,
            end_ms: 0,
            en: l.to_string(),
            sk: None,
            words: None,
        })
        .collect();

    if lines.is_empty() {
        return None;
    }

    Some(LyricsTrack {
        version: 1,
        source: "lrclib".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_lrc_timestamp ----

    #[test]
    fn timestamp_standard_two_digit_frac() {
        // 01:32.45 → 1*60*1000 + 32*1000 + 450 = 92450
        assert_eq!(parse_lrc_timestamp("01:32.45"), Some(92450));
    }

    #[test]
    fn timestamp_three_digit_frac() {
        // 00:05.123 → 5000 + 123 = 5123
        assert_eq!(parse_lrc_timestamp("00:05.123"), Some(5123));
    }

    #[test]
    fn timestamp_one_digit_frac() {
        // 00:10.5 → 10000 + 500 = 10500
        assert_eq!(parse_lrc_timestamp("00:10.5"), Some(10500));
    }

    #[test]
    fn timestamp_no_frac() {
        // 02:03 → 2*60*1000 + 3*1000 = 123000
        assert_eq!(parse_lrc_timestamp("02:03"), Some(123000));
    }

    #[test]
    fn timestamp_invalid_returns_none() {
        assert_eq!(parse_lrc_timestamp("not_a_timestamp"), None);
        assert_eq!(parse_lrc_timestamp(""), None);
        assert_eq!(parse_lrc_timestamp("xx:yy.zz"), None);
    }

    // ---- parse_lrc ----

    #[test]
    fn lrc_full_parse_three_lines() {
        let lrc = "[00:01.50] Amazing grace\n[00:04.20] How sweet the sound\n[00:07.00] That saved a wretch";
        let track = parse_lrc(lrc).expect("should parse");

        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.source, "lrclib");

        // First line
        assert_eq!(track.lines[0].start_ms, 1500);
        assert_eq!(track.lines[0].end_ms, 4200); // = start of next line
        assert_eq!(track.lines[0].en, "Amazing grace");

        // Second line
        assert_eq!(track.lines[1].start_ms, 4200);
        assert_eq!(track.lines[1].end_ms, 7000);
        assert_eq!(track.lines[1].en, "How sweet the sound");

        // Last line: end = start + 5000
        assert_eq!(track.lines[2].start_ms, 7000);
        assert_eq!(track.lines[2].end_ms, 12000);
        assert_eq!(track.lines[2].en, "That saved a wretch");
    }

    #[test]
    fn lrc_skips_empty_text_lines() {
        let lrc = "[00:01.00] First line\n[00:02.00] \n[00:03.00] Third line";
        let track = parse_lrc(lrc).expect("should parse");
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "First line");
        assert_eq!(track.lines[1].en, "Third line");
    }

    #[test]
    fn lrc_skips_malformed_lines_without_closing_bracket() {
        // A line like "[00:02.00 Missing text" has no closing ']' — must be skipped, not abort
        let lrc = "[00:01.00] First line\n[00:02.00 Missing bracket\n[00:03.00] Third line";
        let track = parse_lrc(lrc).expect("should parse despite malformed line");
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "First line");
        assert_eq!(track.lines[1].en, "Third line");
    }

    #[test]
    fn lrc_empty_input_returns_none() {
        assert!(parse_lrc("").is_none());
        assert!(parse_lrc("   \n   ").is_none());
        // Only empty-text lines → also None
        assert!(parse_lrc("[00:01.00] \n[00:02.00] ").is_none());
    }

    // ---- parse_plain ----

    #[test]
    fn plain_basic_parse() {
        let text = "Amazing grace\nHow sweet the sound\nThat saved a wretch";
        let track = parse_plain(text).expect("should parse");
        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.source, "lrclib");
        assert_eq!(track.lines[0].en, "Amazing grace");
        assert_eq!(track.lines[0].start_ms, 0);
        assert_eq!(track.lines[0].end_ms, 0);
        assert_eq!(track.lines[2].en, "That saved a wretch");
    }

    #[test]
    fn plain_filters_empty_lines() {
        let text = "Line one\n\n\nLine two\n   \nLine three";
        let track = parse_plain(text).expect("should parse");
        assert_eq!(track.lines.len(), 3);
    }

    #[test]
    fn plain_empty_returns_none() {
        assert!(parse_plain("").is_none());
        assert!(parse_plain("  \n  \n  ").is_none());
    }
}
