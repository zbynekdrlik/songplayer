//! #250: a video's lyrics register check (V36 columns). The check itself and
//! its findings live in the track (`{yt}_lyrics.json`, `LyricsTrack::register`);
//! the database keeps its version (the stale pass) and its verdict (the API).

use sqlx::SqlitePool;

/// Record the register check of `video_id`'s track on every row of its video
/// (#144 F1: the track is the video's one file). `verdict` NULL = none
/// recorded (an unreadable track, stamped forward so the pass moves on).
pub async fn stamp_register(
    pool: &SqlitePool,
    video_id: i64,
    version: u32,
    verdict: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET lyrics_register_version = ?, lyrics_register_verdict = ? \
         WHERE youtube_id = (SELECT youtube_id FROM videos WHERE id = ?)",
    )
    .bind(version as i64)
    .bind(verdict)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A served song whose track was not checked under the version bound as the
/// first parameter: a row with lyrics, of an active playlist, downloaded,
/// never a dub's Live-Translate subtitles (the second parameter).
const STALE_REGISTER: &str = "SELECT v.id, v.youtube_id FROM videos v JOIN playlists p ON p.id = v.playlist_id \
     WHERE v.has_lyrics = 1 AND v.lyrics_register_version < ? \
           AND p.is_active = 1 AND v.normalized = 1 \
           AND (v.lyrics_source IS NULL OR v.lyrics_source != ?) \
     ORDER BY v.id ASC";

/// The next song the stale pass checks (`(id, youtube_id)`, lowest id first).
pub async fn fetch_next_stale_register(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as(&format!("{STALE_REGISTER} LIMIT 1"))
        .bind(current_version as i64)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_optional(pool)
        .await
}

/// Every song the stale pass would check, lowest id first: a node that
/// checks nothing itself (paid AI off, PP) stamps the ones whose track
/// already carries a peer's check.
pub async fn fetch_stale_registers(
    pool: &SqlitePool,
    current_version: u32,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(STALE_REGISTER)
        .bind(current_version as i64)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_all(pool)
        .await
}

#[cfg(test)]
#[path = "models_register_tests.rs"]
mod tests;
