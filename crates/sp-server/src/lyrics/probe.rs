//! Per-provider text-source availability probe.
//!
//! Returns a per-provider `available` bool + line-count estimate for the
//! six text-source providers used by `gather_sources_impl`. Probes do NOT
//! invoke Claude cleanup or alignment — they reuse each provider's cheap
//! raw-fetch fn so the operator can pre-flight a song before burning
//! Demucs/Gemini cycles in the lyrics worker.
//!
//! Wired into `POST /api/v1/lyrics/probe-sources` (see `api/lyrics.rs`).
//! Design spec: `docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md`.

use serde::Serialize;
use std::path::Path;
use tracing::debug;

use crate::lyrics::{
    description_provider::fetch_raw_description, genius, lrclib, lyrics_ovh,
    spotify_proxy::SpotifyLyricsFetcher, youtube_subs,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub provider: String,
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

/// Probe all six text-source providers without invoking Claude cleanup.
///
/// `ai_client` is accepted for signature symmetry with `gather_sources_impl`
/// but is unused (no Claude call from the probe path).
#[cfg_attr(test, mutants::skip)]
pub async fn probe_sources_impl(
    _ai_client: Option<&crate::ai::client::AiClient>,
    ytdlp_path: &Path,
    cache_dir: &Path,
    client: &reqwest::Client,
    row: &crate::db::models::VideoLyricsRow,
    genius_access_token: &str,
) -> ProbeReport {
    let youtube_id = row.youtube_id.clone();
    let mut probes: Vec<ProbeResult> = Vec::with_capacity(6);

    // 1. yt_subs (manual only — autosub is banned)
    let yt_tmp = std::env::temp_dir().join("sp_yt_subs_probe");
    let _ = tokio::fs::create_dir_all(&yt_tmp).await;
    let yt_subs_result = match youtube_subs::fetch_subtitles(ytdlp_path, &youtube_id, &yt_tmp).await
    {
        Ok(Some(track)) => ProbeResult {
            provider: "yt_subs".into(),
            available: true,
            line_count: track.lines.len(),
            note: format!("manual captions hit ({} lines)", track.lines.len()),
        },
        Ok(None) => ProbeResult {
            provider: "yt_subs".into(),
            available: false,
            line_count: 0,
            note: "no manual captions".into(),
        },
        Err(e) => ProbeResult {
            provider: "yt_subs".into(),
            available: false,
            line_count: 0,
            note: format!("error: {e}"),
        },
    };
    probes.push(yt_subs_result);

    // 2. description (raw fetch only, no Claude)
    let descr_result = match fetch_raw_description(ytdlp_path, &youtube_id, cache_dir).await {
        Ok(Some(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                ProbeResult {
                    provider: "description".into(),
                    available: false,
                    line_count: 0,
                    note: "description present but empty".into(),
                }
            } else {
                let lc = trimmed.lines().filter(|l| !l.trim().is_empty()).count();
                ProbeResult {
                    provider: "description".into(),
                    available: true,
                    line_count: lc,
                    note: format!("description has {lc} non-empty lines"),
                }
            }
        }
        Ok(None) => ProbeResult {
            provider: "description".into(),
            available: false,
            line_count: 0,
            note: "yt-dlp failed".into(),
        },
        Err(e) => ProbeResult {
            provider: "description".into(),
            available: false,
            line_count: 0,
            note: format!("error: {e}"),
        },
    };
    probes.push(descr_result);

    // 3. lyrics.ovh
    let lyrics_ovh_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lyrics_ovh".into(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match lyrics_ovh::fetch_lyrics(client, &row.artist, &row.song).await {
            Ok(Some(lines)) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: true,
                line_count: lines.len(),
                note: format!("hit ({} lines)", lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: false,
                line_count: 0,
                note: "no lyrics found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lyrics_ovh_result);

    // 4. genius
    let genius_result = if genius_access_token.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            available: false,
            line_count: 0,
            note: "no genius token configured".into(),
        }
    } else if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match genius::fetch_lyrics(client, genius_access_token, &row.artist, &row.song).await {
            Ok(Some(track)) => ProbeResult {
                provider: "genius".into(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "genius".into(),
                available: false,
                line_count: 0,
                note: "not found".into(),
            },
            Err(e) => ProbeResult {
                provider: "genius".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(genius_result);

    // 5. lrclib
    let lrclib_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lrclib".into(),
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
                available: false,
                line_count: 0,
                note: "not found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lrclib".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lrclib_result);

    // 6. spotify (only if row has spotify_track_id)
    let spotify_result = if let Some(tid) = row.spotify_track_id.as_deref() {
        let fetcher = SpotifyLyricsFetcher::new();
        match fetcher.fetch(tid).await {
            Ok(Some(track)) => ProbeResult {
                provider: "spotify".into(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "spotify".into(),
                available: false,
                line_count: 0,
                note: "no synced lyrics".into(),
            },
            Err(e) => ProbeResult {
                provider: "spotify".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    } else {
        ProbeResult {
            provider: "spotify".into(),
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
