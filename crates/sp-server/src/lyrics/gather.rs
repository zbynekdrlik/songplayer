//! `gather_sources_impl`: collects every available text + timing source for a
//! single song and returns a `SongContext` ready for the orchestrator.
//!
//! Extracted from `worker.rs` so worker stays under the per-file line cap;
//! behavior, call order, and public signature are unchanged. Unit tests live
//! in `worker_tests.rs` and reach in via `crate::lyrics::worker::gather_sources_impl`,
//! which is re-exported from `worker` for backward compatibility.

use anyhow::Result;
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::lyrics::{
    genius, lrclib, lyrics_ovh, spotify_proxy::SpotifyLyricsFetcher, youtube_subs,
};

/// Returns `true` if any line in the lrclib track has a non-zero `end_ms`,
/// indicating synced (timestamped) lyrics. `lrclib.rs::parse_plain` emits
/// all-zero timing for the `plainLyrics` fallback path, so this detects
/// that case.
pub(crate) fn lrclib_track_has_real_timing(t: &sp_core::lyrics::LyricsTrack) -> bool {
    t.lines.iter().any(|l| l.end_ms > 0)
}

/// Free function containing the `gather_sources` logic so it can be tested
/// without constructing a full `LyricsWorker`.
///
/// mutants::skip: legacy LRCLIB guards + description match guard are
/// exercised end-to-end by `gather_sources_pushes_description_candidate_when_claude_returns_lyrics`
/// and `gather_sources_skips_description_when_claude_returns_empty_array` integration tests
/// (plus the structural call-order test further down); individual mutations in these
/// I/O-bound branches cannot be killed by unit tests without a full mock harness for
/// yt-dlp/LRCLIB, which is out of scope.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn gather_sources_impl(
    ai_client: Option<&crate::ai::client::AiClient>,
    ytdlp_path: &std::path::Path,
    cache_dir: &std::path::Path,
    client: &reqwest::Client,
    row: &crate::db::models::VideoLyricsRow,
    genius_access_token: &str,
) -> Result<crate::lyrics::provider::SongContext> {
    use crate::lyrics::provider::{CandidateText, SongContext};

    let youtube_id = row.youtube_id.clone();
    let audio_path = row
        .audio_file_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_default();

    // 1. Manual yt_subs
    let yt_tmp = std::env::temp_dir().join("sp_yt_subs");
    let _ = tokio::fs::create_dir_all(&yt_tmp).await;
    let yt_subs_track = match youtube_subs::fetch_subtitles(ytdlp_path, &youtube_id, &yt_tmp).await
    {
        Ok(Some(track)) => {
            info!("gather: YT manual subs hit for {youtube_id}");
            Some(track)
        }
        Ok(None) => {
            debug!("gather: no YT manual subs for {youtube_id}");
            None
        }
        Err(e) => {
            warn!("gather: YT sub fetch error for {youtube_id}: {e}");
            None
        }
    };

    // 2. LRCLIB (if song/artist known)
    let lrclib_track = if !row.song.is_empty() && !row.artist.is_empty() {
        let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
        match lrclib::fetch_lyrics(client, &row.artist, &row.song, duration_s).await {
            Ok(Some(track)) => {
                info!("gather: LRCLIB hit for {youtube_id}");
                Some(track)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("gather: LRCLIB error for {youtube_id}: {e}");
                None
            }
        }
    } else {
        None
    };

    // 2a. lyrics.ovh (community lyrics API, no auth, clean plain text).
    // Tried FIRST because the response is already free of HTML, section
    // markers, and contributor banners — no parser fragility.
    let lyrics_ovh_lines = if !row.song.is_empty() && !row.artist.is_empty() {
        match lyrics_ovh::fetch_lyrics(client, &row.artist, &row.song).await {
            Ok(Some(lines)) => {
                info!(%youtube_id, line_count = lines.len(), "gather: lyrics.ovh hit");
                Some(lines)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("gather: lyrics.ovh error for {youtube_id}: {e}");
                None
            }
        }
    } else {
        None
    };

    // 2b. Genius FALLBACK — only consulted when lyrics.ovh did not return a
    // match. Genius has a wider catalog than lyrics.ovh; the prior HTML
    // parser bug (naive `</div>` find truncating multi-container pages,
    // observed 2026-05-11 on planetboom Saints) is fixed by
    // `find_matching_div_close` counting nested div depth.
    let genius_track =
        if lyrics_ovh_lines.is_none() && !row.song.is_empty() && !row.artist.is_empty() {
            match genius::fetch_lyrics(client, genius_access_token, &row.artist, &row.song).await {
                Ok(Some(t)) => {
                    info!(%youtube_id, line_count = t.lines.len(), "gather: Genius fallback hit");
                    Some(t)
                }
                Ok(None) => None,
                Err(e) => {
                    warn!("gather: Genius fallback error for {youtube_id}: {e}");
                    None
                }
            }
        } else {
            None
        };

    // 3. Spotify (operator pasted track URL via dashboard). Authoritative
    //    LINE_SYNCED lyrics for songs the other Tier-1 sources miss
    //    (chant, dense vocal, niche worship). Best-effort; transport /
    //    proxy errors log and skip.
    let spotify_track = if let Some(track_id) = row.spotify_track_id.as_deref() {
        let fetcher = SpotifyLyricsFetcher::new();
        match fetcher.fetch(track_id).await {
            Ok(Some(t)) => {
                info!(%youtube_id, line_count = t.lines.len(), "gather: Spotify hit");
                Some(t)
            }
            Ok(None) => {
                debug!("gather: no Spotify synced lyrics for {youtube_id}");
                None
            }
            Err(crate::lyrics::spotify_proxy::SpotifyError::NotFound) => {
                // 404 from the proxy is common (deleted track / stale ID); not a
                // production warning. Demote to debug so the log stays quiet on
                // catalogs with many old Spotify URLs.
                debug!("gather: Spotify track id not found for {youtube_id}");
                None
            }
            Err(e) => {
                warn!("gather: Spotify error for {youtube_id}: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut candidate_texts: Vec<CandidateText> = Vec::new();

    // 0. Operator-provided override (V15). Highest priority — when an
    //    operator has pasted lyrics for a song, they expect those lines
    //    to drive alignment, not whatever yt_subs/LRCLIB/description the
    //    gather paths produce. Empty/whitespace override is ignored.
    if let Some(raw) = row.lyrics_override_text.as_ref() {
        let lines: Vec<String> = raw
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !lines.is_empty() {
            info!(
                youtube_id = %youtube_id,
                line_count = lines.len(),
                "gather: operator lyrics override present"
            );
            candidate_texts.push(CandidateText {
                source: "override".into(),
                lines,
                has_timing: false,
                line_timings: None,
            });
        }
    }
    // Spotify priority sits between override and other Tier-1 sources;
    // see `claude_merge::source_priority` for the exact ordering.
    if let Some(t) = spotify_track {
        candidate_texts.push(t.into());
    }
    if let Some(t) = &yt_subs_track {
        candidate_texts.push(CandidateText {
            source: "yt_subs".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    }
    if let Some(t) = &lrclib_track {
        // lrclib.rs::parse_plain emits lines with start_ms=0/end_ms=0; only
        // synced lyrics have real timestamps. Detect via the helper so the
        // logic is unit-testable.
        let real_timing = lrclib_track_has_real_timing(t);
        if real_timing {
            candidate_texts.push(CandidateText {
                source: "lrclib".into(),
                lines: t.lines.iter().map(|l| l.en.clone()).collect(),
                has_timing: true,
                line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
            });
        } else {
            let Some(ai) = ai_client else {
                anyhow::bail!(
                    "gather: lrclib-plain candidate present but ai_client is None for {youtube_id}; \
                     cannot run Claude cleanup"
                );
            };
            let raw_blob: String = t
                .lines
                .iter()
                .map(|l| l.en.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            // _v2 cache filename invalidates pre-2026-05-11 caches written
            // under the description-prompt (no dedup / no ad-lib strip).
            let cache_path = cache_dir.join(format!("{youtube_id}_lrclib_cleaned_v2.json"));
            match crate::lyrics::description_provider::clean_lyrics_via_claude(
                ai,
                &row.song,
                &row.artist,
                &raw_blob,
                &cache_path,
                crate::lyrics::description_provider::CleanupMode::ScrapedLyrics,
            )
            .await
            {
                Ok(Some(cleaned)) if !cleaned.is_empty() => {
                    info!(
                        %youtube_id,
                        raw_count = t.lines.len(),
                        cleaned_count = cleaned.len(),
                        "gather: lrclib-plain Claude cleanup complete"
                    );
                    candidate_texts.push(CandidateText {
                        source: "lrclib".into(),
                        lines: cleaned,
                        has_timing: false,
                        line_timings: None,
                    });
                }
                Ok(_) => anyhow::bail!(
                    "gather: lrclib-plain cleanup returned no lyrics for {youtube_id}"
                ),
                Err(e) => {
                    anyhow::bail!("gather: lrclib-plain cleanup failed for {youtube_id}: {e}")
                }
            }
        }
    }
    // lyrics.ovh returns clean plain-text lyrics — no section markers, no
    // contributor banners, no HTML. Push directly as a TextOnly candidate;
    // no Claude cleanup pass needed. Source label "genius" preserves the
    // existing `priority_with_timing` ranking slot for the community-
    // lyrics-website tier; lyrics.ovh and Genius are equivalent at that
    // priority (both fan-curated text references).
    if let Some(lines) = lyrics_ovh_lines {
        candidate_texts.push(CandidateText {
            source: "genius".into(),
            lines,
            has_timing: false,
            line_timings: None,
        });
    } else if let Some(t) = &genius_track {
        // Genius fallback — only reached when lyrics.ovh missed. The HTML
        // parser bug that truncated multi-container pages is fixed; the
        // resulting text may still benefit from Claude cleanup because
        // Genius HTML preserves section-marker artifacts in some pages.
        let Some(ai) = ai_client else {
            anyhow::bail!(
                "gather: genius fallback present but ai_client is None for {youtube_id}; \
                 cannot run Claude cleanup"
            );
        };
        let raw_blob: String = t
            .lines
            .iter()
            .map(|l| l.en.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let cache_path = cache_dir.join(format!("{youtube_id}_genius_cleaned_v2.json"));
        match crate::lyrics::description_provider::clean_lyrics_via_claude(
            ai,
            &row.song,
            &row.artist,
            &raw_blob,
            &cache_path,
            crate::lyrics::description_provider::CleanupMode::ScrapedLyrics,
        )
        .await
        {
            Ok(Some(cleaned)) if !cleaned.is_empty() => {
                info!(
                    %youtube_id,
                    raw_count = t.lines.len(),
                    cleaned_count = cleaned.len(),
                    "gather: genius fallback Claude cleanup complete"
                );
                candidate_texts.push(CandidateText {
                    source: "genius".into(),
                    lines: cleaned,
                    has_timing: false,
                    line_timings: None,
                });
            }
            Ok(_) => {
                anyhow::bail!("gather: genius fallback cleanup returned no lyrics for {youtube_id}")
            }
            Err(e) => anyhow::bail!("gather: genius fallback cleanup failed for {youtube_id}: {e}"),
        }
    }

    // 4. YouTube description lyrics (LLM-extracted). Best-effort.
    if let Some(ai) = ai_client {
        let description_lines = match crate::lyrics::description_provider::fetch_description_lyrics(
            ai,
            ytdlp_path,
            &youtube_id,
            cache_dir,
            &row.song,
            &row.artist,
        )
        .await
        {
            Ok(Some(lines)) if !lines.is_empty() => {
                info!(
                    youtube_id = %youtube_id,
                    line_count = lines.len(),
                    "gather: description lyrics hit"
                );
                Some(lines)
            }
            Ok(_) => {
                debug!("gather: no description lyrics for {youtube_id}");
                None
            }
            Err(e) => {
                warn!("gather: description fetch error for {youtube_id}: {e}");
                None
            }
        };
        if let Some(lines) = description_lines {
            candidate_texts.push(CandidateText {
                source: "description".into(),
                lines,
                has_timing: false,
                line_timings: None,
            });
        }
    }

    if candidate_texts.is_empty() {
        anyhow::bail!("no text sources available for {youtube_id}");
    }

    Ok(SongContext {
        video_id: youtube_id,
        audio_path,
        clean_vocal_path: None, // filled by process_song before orchestrator call
        candidate_texts,
        duration_ms: row.duration_ms.unwrap_or(0) as u64,
    })
}
