//! Per-provider text-source availability probe.
//!
//! Returns a per-provider `available` bool + line-count estimate for the
//! six text-source providers used by `gather_sources_impl`. Two cost tiers:
//! cheap raw-fetch for yt_subs / lyrics.ovh / genius / lrclib / spotify, and
//! a Claude-validated fetch for `description` (because the YouTube description
//! field contains promo / social / sponsor text on most channels — a raw
//! "non-empty after trim" check is not enough to know if it's actually
//! lyrics). The Claude call goes through `fetch_description_lyrics` which
//! caches results in `{youtube_id}_description_lyrics.json`, so subsequent
//! probes are instant.
//!
//! Every `ProbeResult` includes `provider_url` so the operator can click
//! through to the actual page Genius/LRCLIB/lyrics.ovh would have used,
//! catching wrong-artist-match cases by eye before reprocess.
//!
//! Wired into `POST /api/v1/lyrics/probe-sources` (see `api/lyrics.rs`).
//! Design spec: `docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md`.

use serde::Serialize;
use std::path::Path;
use tracing::debug;

use crate::lyrics::{
    description_provider::fetch_description_lyrics, genius, lrclib, lyrics_ovh,
    spotify_proxy::SpotifyLyricsFetcher, youtube_subs,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub provider: String,
    /// User-clickable URL pointing at the page / API endpoint this provider
    /// would consult. Lets the operator manually verify the source before
    /// approving reprocess. Empty string for `spotify` when the row has no
    /// `spotify_track_id`.
    pub provider_url: String,
    pub available: bool,
    pub line_count: usize,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub video_id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub probes: Vec<ProbeResult>,
    pub any_text_source: bool,
    pub recommendation: String,
}

/// Probe all six text-source providers.
///
/// `description` provider validates the YouTube description through Claude
/// when `ai_client` is `Some` (cached on disk so repeat probes are cheap);
/// returns unavailable when `ai_client` is None because raw non-empty text
/// is not a reliable signal — most YouTube descriptions contain only promo
/// / social-link / sponsor text and no actual lyrics.
#[cfg_attr(test, mutants::skip)]
pub async fn probe_sources_impl(
    ai_client: Option<&crate::ai::client::AiClient>,
    ytdlp_path: &Path,
    cache_dir: &Path,
    client: &reqwest::Client,
    row: &crate::db::models::VideoLyricsRow,
    genius_access_token: &str,
) -> ProbeReport {
    let youtube_id = row.youtube_id.clone();
    let yt_video_url = format!("https://youtube.com/watch?v={youtube_id}");
    let mut probes: Vec<ProbeResult> = Vec::with_capacity(6);

    // 1. yt_subs (manual only — autosub is banned)
    let yt_tmp = std::env::temp_dir().join("sp_yt_subs_probe");
    let _ = tokio::fs::create_dir_all(&yt_tmp).await;
    let yt_subs_result = match youtube_subs::fetch_subtitles(ytdlp_path, &youtube_id, &yt_tmp).await
    {
        Ok(Some(track)) => ProbeResult {
            provider: "yt_subs".into(),
            provider_url: yt_video_url.clone(),
            available: true,
            line_count: track.lines.len(),
            note: format!("manual captions hit ({} lines)", track.lines.len()),
        },
        Ok(None) => ProbeResult {
            provider: "yt_subs".into(),
            provider_url: yt_video_url.clone(),
            available: false,
            line_count: 0,
            note: "no manual captions".into(),
        },
        Err(e) => ProbeResult {
            provider: "yt_subs".into(),
            provider_url: yt_video_url.clone(),
            available: false,
            line_count: 0,
            note: format!("error: {e}"),
        },
    };
    probes.push(yt_subs_result);

    // 2. description (Claude-validated — raw non-empty is not enough)
    //
    // 2026-05-13 Jesus Saves id=175 was the trigger: yt description was an
    // Air1 promo blurb + social links, 14 non-empty lines but zero lyrics.
    // Raw-availability check passed; reprocess would have wasted Demucs +
    // whisperx cycles before description_provider cleanup said no lyrics.
    // Probe now consults the same Claude-cleanup path the worker uses, with
    // the same `{youtube_id}_description_lyrics.json` cache so subsequent
    // probes are instant. Without ai_client the probe is conservative and
    // reports unavailable, because honest "unknown" defaults to "skip" per
    // operator preference.
    let descr_result = match ai_client {
        Some(ai) => match fetch_description_lyrics(
            ai,
            ytdlp_path,
            &youtube_id,
            cache_dir,
            &row.song,
            &row.artist,
        )
        .await
        {
            Ok(Some(lines)) if !lines.is_empty() => ProbeResult {
                provider: "description".into(),
                provider_url: yt_video_url.clone(),
                available: true,
                line_count: lines.len(),
                note: format!(
                    "Claude extracted {} lyric lines from description",
                    lines.len()
                ),
            },
            Ok(_) => ProbeResult {
                provider: "description".into(),
                provider_url: yt_video_url.clone(),
                available: false,
                line_count: 0,
                note: "YouTube description has no lyrics (Claude-validated)".into(),
            },
            Err(e) => ProbeResult {
                provider: "description".into(),
                provider_url: yt_video_url.clone(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        },
        None => ProbeResult {
            provider: "description".into(),
            provider_url: yt_video_url.clone(),
            available: false,
            line_count: 0,
            note: "skipped (no ai_client — cannot Claude-validate description)".into(),
        },
    };
    probes.push(descr_result);

    // 3. lyrics.ovh — community lyrics, https://www.lyrics.ovh/
    let lyrics_ovh_url = if row.artist.is_empty() || row.song.is_empty() {
        "https://www.lyrics.ovh/".to_string()
    } else {
        format!(
            "https://api.lyrics.ovh/v1/{}/{}",
            urlencoding::encode(&row.artist),
            urlencoding::encode(&row.song),
        )
    };
    let lyrics_ovh_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lyrics_ovh".into(),
            provider_url: lyrics_ovh_url.clone(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match lyrics_ovh::fetch_lyrics(client, &row.artist, &row.song).await {
            Ok(Some(lines)) => ProbeResult {
                provider: "lyrics_ovh".into(),
                provider_url: lyrics_ovh_url.clone(),
                available: true,
                line_count: lines.len(),
                note: format!("hit ({} lines)", lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "lyrics_ovh".into(),
                provider_url: lyrics_ovh_url.clone(),
                available: false,
                line_count: 0,
                note: "no lyrics found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lyrics_ovh".into(),
                provider_url: lyrics_ovh_url.clone(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lyrics_ovh_result);

    // 4. genius — public search, https://genius.com/
    let genius_search_url = if row.artist.is_empty() || row.song.is_empty() {
        "https://genius.com/".to_string()
    } else {
        format!(
            "https://genius.com/search?q={}",
            urlencoding::encode(&format!("{} {}", row.artist, row.song))
        )
    };
    let genius_result = if genius_access_token.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            provider_url: genius_search_url.clone(),
            available: false,
            line_count: 0,
            note: "no genius token configured".into(),
        }
    } else if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            provider_url: genius_search_url.clone(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match genius::fetch_lyrics(client, genius_access_token, &row.artist, &row.song).await {
            Ok(Some(track)) => ProbeResult {
                provider: "genius".into(),
                provider_url: genius_search_url.clone(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines, strict artist match)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "genius".into(),
                provider_url: genius_search_url.clone(),
                available: false,
                line_count: 0,
                note: "no matching-artist song hit".into(),
            },
            Err(e) => ProbeResult {
                provider: "genius".into(),
                provider_url: genius_search_url.clone(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(genius_result);

    // 5. lrclib — https://lrclib.net/
    let lrclib_url = if row.artist.is_empty() || row.song.is_empty() {
        "https://lrclib.net/".to_string()
    } else {
        let dur = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
        format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}&duration={}",
            urlencoding::encode(&row.artist),
            urlencoding::encode(&row.song),
            dur,
        )
    };
    let lrclib_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lrclib".into(),
            provider_url: lrclib_url.clone(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        let dur = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
        match lrclib::fetch_lyrics(client, &row.artist, &row.song, dur).await {
            Ok(Some(track)) => {
                let has_real_timing = track.lines.iter().any(|l| l.end_ms > 0);
                ProbeResult {
                    provider: "lrclib".into(),
                    provider_url: lrclib_url.clone(),
                    available: true,
                    line_count: track.lines.len(),
                    note: format!(
                        "hit ({} lines, {})",
                        track.lines.len(),
                        if has_real_timing {
                            "synced timing"
                        } else {
                            "plain text only"
                        }
                    ),
                }
            }
            Ok(None) => ProbeResult {
                provider: "lrclib".into(),
                provider_url: lrclib_url.clone(),
                available: false,
                line_count: 0,
                note: "not found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lrclib".into(),
                provider_url: lrclib_url.clone(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lrclib_result);

    // 6. spotify (only if row has spotify_track_id)
    let spotify_url = match row.spotify_track_id.as_deref() {
        Some(tid) => format!("https://open.spotify.com/track/{tid}"),
        None => "https://open.spotify.com/".to_string(),
    };
    let spotify_result = if let Some(tid) = row.spotify_track_id.as_deref() {
        let fetcher = SpotifyLyricsFetcher::new();
        match fetcher.fetch(tid).await {
            Ok(Some(track)) => ProbeResult {
                provider: "spotify".into(),
                provider_url: spotify_url.clone(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "spotify".into(),
                provider_url: spotify_url.clone(),
                available: false,
                line_count: 0,
                note: "no synced lyrics".into(),
            },
            Err(e) => ProbeResult {
                provider: "spotify".into(),
                provider_url: spotify_url.clone(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    } else {
        ProbeResult {
            provider: "spotify".into(),
            provider_url: spotify_url.clone(),
            available: false,
            line_count: 0,
            note: "no spotify_track_id on row".into(),
        }
    };
    probes.push(spotify_result);

    let any_text_source = probes.iter().any(|p| p.available);
    let recommendation = if any_text_source {
        "proceed".to_string()
    } else {
        "skip_no_text_source".to_string()
    };

    debug!(
        video_id = row.id,
        youtube_id = %youtube_id,
        any_text_source,
        "probe: complete"
    );

    ProbeReport {
        video_id: row.id,
        youtube_id,
        song: row.song.clone(),
        artist: row.artist.clone(),
        probes,
        any_text_source,
        recommendation,
    }
}
