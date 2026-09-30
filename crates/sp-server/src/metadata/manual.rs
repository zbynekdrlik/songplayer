//! #136 (ROZHODNUTÉ 5908227964): an operator's title correction belongs to
//! the VIDEO, not to the playlist row, and it is final.
//!
//! One YouTube video is one song with one title (owner rule: one app, one
//! behaviour), yet the same video in two playlists is two `videos` rows (14
//! ids had 2+ rows on the box, 30.9.2026). So a title PATCH of one row
//! (`PATCH /api/v1/videos/{id}`, `api/routes.rs::patch_video`) is spread by
//! [`apply_to_video`] to every row of its `youtube_id` — `song`, `artist`,
//! `metadata_source = 'manual'`, `gemini_failed = 0` — and each row's files
//! are renamed after the correction through `cache::rename_song_files`, the
//! one rename path (`.claude/rules/song-files.md`). A `'manual'` row is out
//! of the metadata repair's queue (`health::REPAIR_QUEUE_WHERE`), so nothing
//! renames it back.

use std::path::Path;

use sqlx::SqlitePool;
use tracing::info;

use crate::downloader::cache::{SongFiles, rename_song_files};

/// Spread row `video_db_id`'s title (the correction its PATCH just wrote) to
/// every row of its YouTube video, and rename each row's files after it in
/// `cache_dir` (no `_gf`: a correction is no parser title). Each row's set is
/// read right before its move, never from a snapshot (an earlier row of the
/// loop may have moved the same files), and recorded on every row that
/// recorded it (`SongColumns::record`). A row with no files yet (not
/// downloaded) takes the title and records none. The caller holds
/// `cache::SONG_FILES` from its own UPDATE to here, so a repair or a
/// re-link never interleaves. A row that no longer exists is nothing to do.
pub async fn apply_to_video(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_db_id: i64,
) -> Result<(), sqlx::Error> {
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT youtube_id, COALESCE(song, ''), artist FROM videos WHERE id = ?")
            .bind(video_db_id)
            .fetch_optional(pool)
            .await?;
    let Some((youtube_id, song, artist)) = row else {
        return Ok(());
    };
    let spread = sqlx::query(
        "UPDATE videos SET song = ?, artist = ?, metadata_source = 'manual', gemini_failed = 0 \
         WHERE youtube_id = ?",
    )
    .bind(&song)
    .bind(&artist)
    .bind(&youtube_id)
    .execute(pool)
    .await?;
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ? ORDER BY id")
            .bind(&youtube_id)
            .fetch_all(pool)
            .await?;
    for id in ids {
        let (file_path, audio_file_path): (String, Option<String>) = sqlx::query_as(
            "SELECT COALESCE(file_path, ''), audio_file_path FROM videos WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        let old = SongFiles::recorded(&file_path, audio_file_path.as_deref());
        if old.is_empty() {
            continue; // not downloaded yet: no files to name
        }
        let artist_name = artist.as_deref().unwrap_or("");
        let new = old.named(cache_dir, &song, artist_name, &youtube_id, false);
        let files = rename_song_files(&youtube_id, &old, &new).columns();
        files
            .record(pool, &youtube_id, &file_path, audio_file_path.as_deref())
            .await?;
    }
    info!(
        video_db_id,
        youtube_id = %youtube_id,
        song = %song,
        artist = ?artist,
        rows = spread.rows_affected(),
        "metadata: the operator's title correction applies to every row of the video"
    );
    Ok(())
}
