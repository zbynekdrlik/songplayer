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
pub(crate) async fn persist_lyrics_track(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    video_id: i64,
    track: &LyricsTrack,
    pipeline_version: u32,
) -> Result<()> {
    let json_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let json_bytes = serde_json::to_vec(track)?;
    tokio::fs::write(&json_path, &json_bytes).await?;

    // Pick the alignment-model literal for this success path — see
    // `alignment_model_for_source`'s doc comment for the precedence.
    let alignment_model =
        crate::lyrics::worker_reference::alignment_model_for_source(&track.source);
    crate::db::models::mark_video_lyrics_complete(
        pool,
        video_id,
        &track.source,
        pipeline_version,
        None,
        alignment_model,
    )
    .await?;
    Ok(())
}
