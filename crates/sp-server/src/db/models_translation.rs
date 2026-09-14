//! Per-song SK translation gender + version queries (#152).
//!
//! Split out of `models.rs` to keep that file under the 1000-line airuleset
//! cap; re-exported from `models.rs` via `pub use models_translation::*;` so
//! call sites use `crate::db::models::set_translation_gender`, etc.
//!
//! Independent of `lyrics_pipeline_version` / alignment: setting a gender or
//! bumping `LYRICS_TRANSLATION_VERSION` triggers a translation-only re-pass
//! (one Claude call, `sk` lines rewritten in place), never re-alignment.

use super::VideoLyricsRow;
use sqlx::SqlitePool;

/// Read `videos.lyrics_translation_gender` for one video. Returns `None` when
/// the column is NULL (auto / default) or the row does not exist. `"m"` =
/// masculine, `"f"` = feminine.
pub async fn get_translation_gender(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let v: Option<Option<String>> =
        sqlx::query_scalar("SELECT lyrics_translation_gender FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_optional(pool)
            .await?;
    Ok(v.flatten())
}

/// Set (or clear with `None`) `videos.lyrics_translation_gender` for one video
/// AND reset its `lyrics_translation_version` to 0, so the stale-translation
/// worker pass re-translates the song under the new gender immediately (no
/// manual priority needed). `gender` must already be validated to
/// `None` / `Some("m")` / `Some("f")` by the HTTP handler. Returns `true` iff a
/// row was updated (`false` → no such video → the handler maps that to 404).
pub async fn set_translation_gender(
    pool: &SqlitePool,
    video_id: i64,
    gender: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE videos SET lyrics_translation_gender = ?1, lyrics_translation_version = 0 \
         WHERE id = ?2",
    )
    .bind(gender)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Stamp the SK translation version onto a video after a successful (re)
/// translation so the stale-translation selector skips it until
/// `LYRICS_TRANSLATION_VERSION` bumps again. Never touches alignment or
/// `lyrics_pipeline_version`.
pub async fn stamp_translation_version(
    pool: &SqlitePool,
    video_id: i64,
    version: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET lyrics_translation_version = ?1 WHERE id = ?2")
        .bind(version as i64)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Pick the next persisted song whose SK translation is stale (produced under
/// an older `LYRICS_TRANSLATION_VERSION`). Only `has_lyrics = 1` rows on an
/// active, normalized playlist are eligible — nothing to translate otherwise;
/// oldest (lowest id) first. Returns `None` when every active song is current,
/// so the worker does nothing this tick. The column list matches
/// `VideoLyricsRow` exactly so the worker can re-run translation with the same
/// row shape the alignment pipeline uses.
#[cfg_attr(test, mutants::skip)] // Selection lives in the SQL WHERE/ORDER; the
// Rust glue (bind/await/Ok) is covered by the picks/skips/oldest-first tests
// in models_tests_translation.rs.
pub async fn fetch_next_stale_translation(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<VideoLyricsRow>, sqlx::Error> {
    let row = sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url, v.lyrics_override_text, v.lyrics_time_offset_ms, \
                v.spotify_track_id, v.spotify_resolved_at \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 AND v.lyrics_translation_version < ? \
               AND p.is_active = 1 AND v.normalized = 1 \
         ORDER BY v.id ASC LIMIT 1",
    )
    .bind(current_version as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[path = "models_tests_translation.rs"]
#[cfg(test)]
mod tests_translation;
