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

use crate::lyrics::{genius, lrclib, youtube_subs};

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

    // 2a. Genius (documented API + public lyric-page scrape). No timing.
    let genius_track = if !row.song.is_empty() && !row.artist.is_empty() {
        match genius::fetch_lyrics(client, genius_access_token, &row.artist, &row.song).await {
            Ok(Some(t)) => {
                info!(%youtube_id, line_count = t.lines.len(), "gather: Genius hit");
                Some(t)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("gather: Genius error for {youtube_id}: {e}");
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
    if let Some(t) = &yt_subs_track {
        candidate_texts.push(CandidateText {
            source: "yt_subs".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    }
    if let Some(t) = &lrclib_track {
        candidate_texts.push(CandidateText {
            source: "lrclib".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    }
    if let Some(t) = &genius_track {
        candidate_texts.push(CandidateText {
            source: "genius".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: false,
            line_timings: None,
        });
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
