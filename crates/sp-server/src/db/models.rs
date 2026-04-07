//! Query functions that map between SQLite rows and `sp_core::models` types.

use sp_core::models::{Playlist, Video};
use sqlx::{Row, SqlitePool};

// ---------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------

/// Return all playlists where `is_active = 1`.
pub async fn get_active_playlists(pool: &SqlitePool) -> Result<Vec<Playlist>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, is_active FROM playlists WHERE is_active = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| Playlist {
            id: r.get("id"),
            name: r.get("name"),
            youtube_playlist_id: r.get("youtube_url"),
            enabled: r.get::<i32, _>("is_active") != 0,
        })
        .collect())
}

/// Insert a new playlist and return the created model.
pub async fn insert_playlist(
    pool: &SqlitePool,
    name: &str,
    youtube_url: &str,
) -> Result<Playlist, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO playlists (name, youtube_url) VALUES (?, ?) RETURNING id, name, youtube_url, is_active",
    )
    .bind(name)
    .bind(youtube_url)
    .fetch_one(pool)
    .await?;

    Ok(Playlist {
        id: row.get("id"),
        name: row.get("name"),
        youtube_playlist_id: row.get("youtube_url"),
        enabled: row.get::<i32, _>("is_active") != 0,
    })
}

// ---------------------------------------------------------------------------
// Videos
// ---------------------------------------------------------------------------

/// Return all videos belonging to a playlist.
pub async fn get_videos_for_playlist(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<Video>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, playlist_id, youtube_id, title, song, artist,
                duration_ms, file_path, normalized, gemini_failed
         FROM videos WHERE playlist_id = ? ORDER BY id",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_video).collect())
}

/// Insert or update a video keyed on (playlist_id, youtube_id).
/// On conflict the title is updated. Returns the resulting row.
pub async fn upsert_video(
    pool: &SqlitePool,
    playlist_id: i64,
    youtube_id: &str,
    title: Option<&str>,
) -> Result<Video, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title)
         VALUES (?, ?, ?)
         ON CONFLICT(playlist_id, youtube_id) DO UPDATE SET title = excluded.title
         RETURNING id, playlist_id, youtube_id, title, song, artist,
                   duration_ms, file_path, normalized, gemini_failed",
    )
    .bind(playlist_id)
    .bind(youtube_id)
    .bind(title)
    .fetch_one(pool)
    .await?;

    Ok(row_to_video(&row))
}

fn row_to_video(r: &sqlx::sqlite::SqliteRow) -> Video {
    Video {
        id: r.get("id"),
        playlist_id: r.get("playlist_id"),
        youtube_id: r.get("youtube_id"),
        title: r.get::<Option<String>, _>("title").unwrap_or_default(),
        song: r.get("song"),
        artist: r.get("artist"),
        duration_ms: r.get("duration_ms"),
        cached: r.get::<Option<String>, _>("file_path").is_some(),
        normalized: r.get::<i32, _>("normalized") != 0,
        gemini_failed: r.get::<i32, _>("gemini_failed") != 0,
    }
}

// ---------------------------------------------------------------------------
// Play history
// ---------------------------------------------------------------------------

/// Record that a video was played now.
pub async fn record_play(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO play_history (playlist_id, video_id) VALUES (?, ?)")
        .bind(playlist_id)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Return youtube_ids of videos in the playlist that have never been played.
pub async fn get_unplayed_video_ids(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT v.youtube_id
         FROM videos v
         LEFT JOIN play_history ph ON ph.video_id = v.id
         WHERE v.playlist_id = ? AND ph.id IS NULL
         ORDER BY v.id",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(|r| r.get("youtube_id")).collect())
}

/// Return IDs (from videos table) of normalized videos in a playlist.
pub async fn get_normalized_video_ids(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows =
        sqlx::query("SELECT id FROM videos WHERE playlist_id = ? AND normalized = 1 ORDER BY id")
            .bind(playlist_id)
            .fetch_all(pool)
            .await?;

    Ok(rows.iter().map(|r| r.get("id")).collect())
}

/// Return IDs (from videos table) of normalized videos that have not been played.
pub async fn get_unplayed_normalized_video_ids(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT v.id
         FROM videos v
         LEFT JOIN play_history ph ON ph.video_id = v.id
         WHERE v.playlist_id = ? AND v.normalized = 1 AND ph.id IS NULL
         ORDER BY v.id",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(|r| r.get("id")).collect())
}

/// Clear all play history for a playlist, allowing videos to be replayed.
pub async fn clear_play_history(pool: &SqlitePool, playlist_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM play_history WHERE playlist_id = ?")
        .bind(playlist_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Get a setting value by key, or `None` if not set.
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| r.get("value")))
}

/// Insert or update a setting.
pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
