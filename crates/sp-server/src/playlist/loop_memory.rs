//! #229 follow-up: Loop mode's song survives a restart.
//!
//! Loop repeats the pipeline's current song, which lives in memory only, so
//! after a SongPlayer restart (a deploy, a reboot) the first press of a Loop
//! playlist picked a random song — PP's yt90s, set to play "We Raise" only,
//! played another (owner, 11.10.2026). The engine now remembers each
//! playlist's new current song in its row (V37 `playlists.loop_video_id`),
//! and Loop takes it when it has no current song in memory.

use sqlx::SqlitePool;
use tracing::warn;

/// Remember `video_id` as `playlist_id`'s current song.
pub async fn remember(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE playlists SET loop_video_id = ? WHERE id = ?")
        .bind(video_id)
        .bind(playlist_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// [`remember`], a failure logged (the song still plays; only a restart
/// would forget it).
pub async fn remember_logged(pool: &SqlitePool, playlist_id: i64, video_id: i64) {
    if let Err(e) = remember(pool, playlist_id, video_id).await {
        warn!(playlist_id, video_id, %e, "loop memory: the current song was not saved");
    }
}

/// `playlist_id`'s remembered song while it can still play there: a
/// downloaded video of that playlist that is still in its YouTube playlist
/// (#260: not marked removed), or one of a custom playlist's items (an
/// operator's set list, kept whatever the mark). `None` when nothing is
/// remembered or the song left the playlist.
pub async fn remembered(pool: &SqlitePool, playlist_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT v.id FROM playlists p JOIN videos v ON v.id = p.loop_video_id \
         WHERE p.id = ? AND v.normalized = 1 \
           AND (v.playlist_id = p.id OR EXISTS ( \
                SELECT 1 FROM playlist_items i \
                WHERE i.playlist_id = p.id AND i.video_id = v.id))",
    )
    .bind(playlist_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
#[path = "loop_memory_tests.rs"]
mod tests;
