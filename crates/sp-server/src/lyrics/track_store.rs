//! Shared persistence for a finished lyrics track (#182).
//!
//! The JSON sidecar the wall/dashboard read + the `videos` row columns are
//! written by ONE function so the lyrics worker (`worker.rs`) and the dub
//! subtitle store (`dabing/subtitles_store.rs`) never drift into two writers with
//! different semantics.

use std::path::Path;

use anyhow::Result;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;

/// Persist `track` for `youtube_id`: write the `{youtube_id}_lyrics.json` sidecar
/// (what the wall + dashboard read) and stamp the `videos` row
/// (`has_lyrics = 1`, `lyrics_source`, `lyrics_pipeline_version`, …) via
/// [`crate::db::models::mark_video_lyrics_complete`]. `quality_score` is `None`
/// (the current pipeline keeps no audit log); the alignment-model literal is
/// derived from `track.source`.
#[cfg_attr(test, mutants::skip)] // JSON write + one UPDATE; covered by the lyrics
// worker + dabing subtitle integration paths (mark_video_lyrics_complete itself
// is likewise skipped in db/models.rs).
pub(crate) async fn persist_lyrics_track(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    video_id: i64,
    track: &LyricsTrack,
    pipeline_version: u32,
) -> Result<()> {
    persist_lyrics_json(
        pool,
        cache_dir,
        youtube_id,
        video_id,
        track,
        &track.source,
        pipeline_version,
    )
    .await
}

/// [`persist_lyrics_track`] with the sidecar body given as any serializable
/// value that serializes AS a [`LyricsTrack`] plus extra top-level fields (the
/// dub subtitle track adds its builder version, #184 H5); `source` is that
/// track's `source`. The ONE writer behind both callers.
#[cfg_attr(test, mutants::skip)] // JSON write + one UPDATE — see persist_lyrics_track.
pub(crate) async fn persist_lyrics_json<T: serde::Serialize>(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    video_id: i64,
    body: &T,
    source: &str,
    pipeline_version: u32,
) -> Result<()> {
    let json_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let json_bytes = serde_json::to_vec(body)?;
    tokio::fs::write(&json_path, &json_bytes).await?;

    // Pick the alignment-model literal for this success path — see
    // `alignment_model_for_source`'s doc comment for the precedence.
    let alignment_model = crate::lyrics::worker_reference::alignment_model_for_source(source);
    crate::db::models::mark_video_lyrics_complete(
        pool,
        video_id,
        source,
        pipeline_version,
        None,
        alignment_model,
    )
    .await?;
    Ok(())
}
