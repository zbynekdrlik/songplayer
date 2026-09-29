//! LRCLIB title search (#144): `GET /api/search?track_name=<title>` returns
//! the records LRCLIB holds under a title, whatever the artist. A cover
//! carries the COVER artist in its metadata, so the artist+title lookup
//! (`lrclib::fetch_lyrics`, `/api/get`) misses the original's records; the
//! title search finds them, and `title_search` chooses among them by what is
//! sung.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use sp_core::lyrics::LyricsTrack;
use tracing::debug;

use crate::lyrics::lrclib::{is_duration_acceptable, parse_lrc, parse_plain, user_agent};

pub const LRCLIB_SEARCH_URL: &str = "https://lrclib.net/api/search";
/// A title-search record is scored only when its duration is within this
/// many seconds of the song's (#144 design) — a different recording of the
/// same title is often a different arrangement.
pub const TITLE_DURATION_TOLERANCE_SECS: u32 = 15;
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// One `/api/search` record (only the fields read). Unknown fields are
/// skipped by serde without building a `serde_json::Value`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRecord {
    id: i64,
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    duration: Option<f32>,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    synced_lyrics: Option<String>,
    #[serde(default)]
    plain_lyrics: Option<String>,
}

/// One LRCLIB record found by title.
#[derive(Debug, Clone, PartialEq)]
pub struct LrclibTitleHit {
    pub id: i64,
    pub artist: String,
    pub track: String,
    pub duration_s: Option<f32>,
    /// The synced lyrics (`parse_lrc`, the record's own line starts) when
    /// LRCLIB has them, else the plain lyrics (`parse_plain`, no timing).
    pub lyrics: LyricsTrack,
    pub synced: bool,
}

/// Search `search_url` (`LRCLIB_SEARCH_URL` in production, via
/// `title_search::TitleSearchEndpoints`) by `track` title and keep the
/// records worth scoring (`hits_from_records`). An empty title searches
/// nothing.
pub(crate) async fn search_by_title_at(
    client: &Client,
    search_url: &str,
    track: &str,
    duration_s: Option<u32>,
) -> Result<Vec<LrclibTitleHit>> {
    let track = track.trim();
    if track.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{search_url}?track_name={}", urlencoding::encode(track));
    debug!(%url, "LRCLIB title search");
    let records: Vec<SearchRecord> = client
        .get(&url)
        .header("User-Agent", user_agent())
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(hits_from_records(records, duration_s))
}

/// The records worth scoring: not instrumental, within the duration
/// tolerance (a record or a song with no known duration is kept — the
/// transcript score decides), with a non-empty lyric. Synced lyrics are
/// preferred over plain.
fn hits_from_records(records: Vec<SearchRecord>, duration_s: Option<u32>) -> Vec<LrclibTitleHit> {
    records
        .into_iter()
        .filter(|r| !r.instrumental && within_duration(r.duration, duration_s))
        .filter_map(|r| {
            let synced = r.synced_lyrics.as_deref().and_then(parse_lrc);
            let (lyrics, is_synced) = match synced {
                Some(track) => (track, true),
                None => (r.plain_lyrics.as_deref().and_then(parse_plain)?, false),
            };
            Some(LrclibTitleHit {
                id: r.id,
                artist: r.artist_name,
                track: r.track_name,
                duration_s: r.duration,
                lyrics,
                synced: is_synced,
            })
        })
        .collect()
}

fn within_duration(record_s: Option<f32>, song_s: Option<u32>) -> bool {
    match (record_s, song_s) {
        (Some(record), Some(song)) => {
            is_duration_acceptable(record, song, TITLE_DURATION_TOLERANCE_SECS)
        }
        _ => true,
    }
}

#[cfg(test)]
#[path = "lrclib_search_tests.rs"]
mod tests;
