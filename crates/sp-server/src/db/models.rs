//! Query functions that map between SQLite rows and `sp_core::models` types.

use sp_core::models::{Playlist, Video};
use sqlx::{Row, SqlitePool};

// ---------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------

/// Return all playlists where `is_active = 1`.
pub async fn get_active_playlists(pool: &SqlitePool) -> Result<Vec<Playlist>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, is_active,
                playback_mode, kind, current_position
         FROM playlists WHERE is_active = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| Playlist {
            id: r.get("id"),
            name: r.get("name"),
            youtube_url: r.get("youtube_url"),
            ndi_output_name: r.get::<String, _>("ndi_output_name"),
            playback_mode: r.get::<String, _>("playback_mode"),
            is_active: r.get::<i32, _>("is_active") != 0,
            kind: r.get::<String, _>("kind"),
            current_position: r.get::<i64, _>("current_position"),
            ..Default::default()
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
        "INSERT INTO playlists (name, youtube_url)
         VALUES (?, ?)
         RETURNING id, name, youtube_url, is_active, playback_mode, kind, current_position",
    )
    .bind(name)
    .bind(youtube_url)
    .fetch_one(pool)
    .await?;

    Ok(Playlist {
        id: row.get("id"),
        name: row.get("name"),
        youtube_url: row.get("youtube_url"),
        playback_mode: row.get::<String, _>("playback_mode"),
        is_active: row.get::<i32, _>("is_active") != 0,
        kind: row.get::<String, _>("kind"),
        current_position: row.get::<i64, _>("current_position"),
        ..Default::default()
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

/// Return the file_path for a video by its ID, or `None` if not normalized.
pub async fn get_video_file_path(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT file_path FROM videos WHERE id = ? AND normalized = 1")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.and_then(|r| r.get("file_path")))
}

/// Update a video row with both sidecar paths after a successful download.
#[allow(clippy::too_many_arguments)]
pub async fn mark_video_processed_pair(
    pool: &SqlitePool,
    video_db_id: i64,
    song: &str,
    artist: &str,
    metadata_source: &str,
    gemini_failed: bool,
    video_path: &str,
    audio_path: &str,
) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        "UPDATE videos
         SET song = ?, artist = ?, metadata_source = ?,
             gemini_failed = ?, file_path = ?, audio_file_path = ?, normalized = 1
         WHERE id = ?",
    )
    .bind(song)
    .bind(artist)
    .bind(metadata_source)
    .bind(gemini_failed as i32)
    .bind(video_path)
    .bind(audio_path)
    .bind(video_db_id)
    .execute(pool)
    .await?;
    debug_assert_eq!(
        result.rows_affected(),
        1,
        "mark_video_processed_pair: expected 1 row affected for id={video_db_id}, got {}",
        result.rows_affected()
    );
    Ok(())
}

/// Return both sidecar paths for a normalized video, or `None`.
pub async fn get_song_paths(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT file_path, audio_file_path FROM videos WHERE id = ? AND normalized = 1",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        let v: Option<String> = r.get("file_path");
        let a: Option<String> = r.get("audio_file_path");
        match (v, a) {
            (Some(vp), Some(ap)) => Some((vp, ap)),
            _ => None,
        }
    }))
}

/// Return the song and artist for a video (for title display).
pub async fn get_video_metadata(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| {
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        (song, artist)
    }))
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

// ---------------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------------

/// A video row with the fields needed by the lyrics worker.
#[derive(Debug, sqlx::FromRow)]
pub struct VideoLyricsRow {
    pub id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub audio_file_path: Option<String>,
    pub youtube_url: String,
}

/// Mark a video's lyrics status and source.
#[cfg_attr(test, mutants::skip)]
pub async fn mark_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    has_lyrics: bool,
    lyrics_source: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET has_lyrics = ?, lyrics_source = ? WHERE id = ?")
        .bind(has_lyrics as i32)
        .bind(lyrics_source)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Persist a successful lyrics processing run: sets has_lyrics=1, records source,
/// pipeline_version, quality_score, and clears manual_priority — all in one query.
///
/// `quality_score` is `None` for fallback paths (e.g. ensemble timeout) to avoid
/// writing 0.0 which would poison the `ORDER BY lyrics_quality_score ASC NULLS FIRST`
/// stale-bucket selector — songs with 0.0 score sort before all real scores.
#[cfg_attr(test, mutants::skip)] // single UPDATE; covered by integration test below
pub async fn mark_video_lyrics_complete(
    pool: &SqlitePool,
    video_id: i64,
    source: &str,
    pipeline_version: u32,
    quality_score: Option<f32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = ?, \
         lyrics_pipeline_version = ?, lyrics_quality_score = ?, \
         lyrics_manual_priority = 0 WHERE id = ?",
    )
    .bind(source)
    .bind(pipeline_version as i64)
    .bind(quality_score.map(|q| q as f64))
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return (total, processed, pending) lyrics counts for active playlists.
#[cfg_attr(test, mutants::skip)]
pub async fn get_lyrics_status(pool: &SqlitePool) -> Result<(i64, i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "SELECT \
         COUNT(*) as total, \
         SUM(CASE WHEN has_lyrics = 1 THEN 1 ELSE 0 END) as processed, \
         SUM(CASE WHEN has_lyrics = 0 AND normalized = 1 THEN 1 ELSE 0 END) as pending \
         FROM videos v \
         JOIN playlists p ON p.id = v.playlist_id \
         WHERE p.is_active = 1",
    )
    .fetch_one(pool)
    .await?;
    let total: i64 = row.get("total");
    let processed: i64 = row.try_get("processed").unwrap_or(0);
    let pending: i64 = row.try_get("pending").unwrap_or(0);
    Ok((total, processed, pending))
}

/// Get next video that has lyrics but is missing SK translation.
#[cfg_attr(test, mutants::skip)]
pub async fn get_next_video_missing_translation(
    pool: &SqlitePool,
    cache_dir: &std::path::Path,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT v.id, v.youtube_id \
         FROM videos v \
         JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 AND p.is_active = 1 \
         ORDER BY v.id",
    )
    .fetch_all(pool)
    .await?;

    for (id, youtube_id) in rows {
        let path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Ok(track) = serde_json::from_str::<sp_core::lyrics::LyricsTrack>(&content) {
            let has_sk = track.lines.iter().any(|l| l.sk.is_some());
            if !has_sk {
                return Ok(Some((id, youtube_id)));
            }
        }
    }
    Ok(None)
}

/// Reset lyrics fields for a video so it will be re-processed.
#[cfg_attr(test, mutants::skip)]
pub async fn reset_video_lyrics(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Custom playlist items
// ---------------------------------------------------------------------------

/// A single item in a custom playlist's set list.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlaylistItem {
    pub position: i64,
    pub video_id: i64,
}

/// Append a video to a custom playlist's set list. Returns the assigned
/// position. Errors if `(playlist_id, video_id)` already exists.
pub async fn append_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let next_pos: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_items WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO playlist_items (playlist_id, video_id, position) VALUES (?, ?, ?)")
        .bind(playlist_id)
        .bind(video_id)
        .bind(next_pos)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(next_pos)
}

/// Remove a video from a custom playlist's set list and compact positions
/// so there are no gaps afterwards.
pub async fn remove_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM playlist_items WHERE playlist_id = ? AND video_id = ?")
        .bind(playlist_id)
        .bind(video_id)
        .execute(&mut *tx)
        .await?;

    // Compact: rewrite positions 0..N-1 preserving order. Two-step to avoid
    // PRIMARY KEY collisions: first negate all positions, then assign
    // sequential non-negative positions based on the negated ordering.
    sqlx::query(
        "UPDATE playlist_items SET position = -position - 1
         WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query(
        "SELECT video_id FROM playlist_items
         WHERE playlist_id = ? ORDER BY position DESC",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;
    for (new_pos, r) in rows.iter().enumerate() {
        let vid: i64 = r.get("video_id");
        sqlx::query(
            "UPDATE playlist_items SET position = ?
             WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(new_pos as i64)
        .bind(playlist_id)
        .bind(vid)
        .execute(&mut *tx)
        .await?;
    }

    // Clamp current_position to the new valid range.
    sqlx::query(
        "UPDATE playlists
         SET current_position = MIN(current_position,
             COALESCE((SELECT MAX(position) FROM playlist_items WHERE playlist_id = ?), 0))
         WHERE id = ?",
    )
    .bind(playlist_id)
    .bind(playlist_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// List all items of a custom playlist in position order.
pub async fn list_playlist_items(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<PlaylistItem>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT position, video_id FROM playlist_items
         WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| PlaylistItem {
            position: r.get("position"),
            video_id: r.get("video_id"),
        })
        .collect())
}

/// Look up the position of a video within a custom playlist.
pub async fn position_for_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT position FROM playlist_items
         WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(playlist_id)
    .bind(video_id)
    .fetch_optional(pool)
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn setup_with_video() -> (SqlitePool, i64) {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Insert an unnormalized video row; the tests will mark it processed.
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized) VALUES (1, 'yt123', 't', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = sqlx::query("SELECT id FROM videos WHERE youtube_id = 'yt123'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let id: i64 = row.get("id");
        (pool, id)
    }

    #[tokio::test]
    async fn mark_video_processed_pair_writes_both_sidecar_paths() {
        let (pool, id) = setup_with_video().await;

        mark_video_processed_pair(
            &pool,
            id,
            "Amazing Grace",
            "Chris Tomlin",
            "gemini",
            false,
            "/cache/S_A_yt12345678_normalized_video.mp4",
            "/cache/S_A_yt12345678_normalized_audio.flac",
        )
        .await
        .unwrap();

        let row = sqlx::query(
            "SELECT song, artist, metadata_source, gemini_failed, file_path, audio_file_path, normalized
             FROM videos WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.get::<String, _>("song"), "Amazing Grace");
        assert_eq!(row.get::<String, _>("artist"), "Chris Tomlin");
        assert_eq!(row.get::<String, _>("metadata_source"), "gemini");
        assert_eq!(row.get::<i64, _>("gemini_failed"), 0);
        assert_eq!(
            row.get::<String, _>("file_path"),
            "/cache/S_A_yt12345678_normalized_video.mp4"
        );
        assert_eq!(
            row.get::<String, _>("audio_file_path"),
            "/cache/S_A_yt12345678_normalized_audio.flac"
        );
        assert_eq!(row.get::<i64, _>("normalized"), 1);
    }

    #[tokio::test]
    async fn mark_video_processed_pair_stores_gemini_failed_flag() {
        let (pool, id) = setup_with_video().await;
        mark_video_processed_pair(
            &pool,
            id,
            "S",
            "A",
            "parser",
            true,
            "/cache/v.mp4",
            "/cache/a.flac",
        )
        .await
        .unwrap();
        let gf: i64 = sqlx::query("SELECT gemini_failed FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("gemini_failed");
        assert_eq!(gf, 1);
    }

    #[tokio::test]
    async fn get_song_paths_returns_both_when_normalized() {
        let (pool, id) = setup_with_video().await;
        mark_video_processed_pair(
            &pool,
            id,
            "S",
            "A",
            "parser",
            false,
            "/cache/video-path.mp4",
            "/cache/audio-path.flac",
        )
        .await
        .unwrap();

        let result = get_song_paths(&pool, id).await.unwrap();
        assert_eq!(
            result,
            Some((
                "/cache/video-path.mp4".to_string(),
                "/cache/audio-path.flac".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn get_song_paths_returns_none_when_unnormalized() {
        let (pool, id) = setup_with_video().await;
        // Row is unnormalized by default from setup_with_video.
        let result = get_song_paths(&pool, id).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn get_song_paths_returns_none_when_audio_missing() {
        let (pool, id) = setup_with_video().await;
        // Mark normalized with only the video path; leave audio_file_path NULL.
        sqlx::query(
            "UPDATE videos SET normalized = 1, file_path = '/cache/v.mp4', audio_file_path = NULL
             WHERE id = ?",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let result = get_song_paths(&pool, id).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn get_song_paths_returns_none_for_nonexistent_id() {
        let (pool, _) = setup_with_video().await;
        let result = get_song_paths(&pool, 9999).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn mark_video_lyrics_complete_writes_all_fields() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_manual_priority) \
                     VALUES (1, 99, 'abc', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        mark_video_lyrics_complete(&pool, 1, "ensemble:qwen3+autosub", 2, Some(0.85))
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, \
             lyrics_quality_score, lyrics_manual_priority FROM videos WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.get::<i64, _>("has_lyrics"), 1);
        assert_eq!(
            row.get::<String, _>("lyrics_source"),
            "ensemble:qwen3+autosub"
        );
        assert_eq!(row.get::<i64, _>("lyrics_pipeline_version"), 2);
        assert!((row.get::<f64, _>("lyrics_quality_score") - 0.85).abs() < 1e-3);
        assert_eq!(
            row.get::<i64, _>("lyrics_manual_priority"),
            0,
            "manual_priority must be cleared on successful processing"
        );
    }

    #[tokio::test]
    async fn mark_complete_with_none_quality_writes_null_not_zero() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
                     VALUES (1, 99, 'abc', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        mark_video_lyrics_complete(&pool, 1, "yt_subs", 2, None)
            .await
            .unwrap();

        let q: Option<f64> =
            sqlx::query_scalar("SELECT lyrics_quality_score FROM videos WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            q, None,
            "fallback path must write NULL, not 0.0 — 0.0 poisons the NULLS FIRST queue ordering"
        );
    }

    #[tokio::test]
    async fn get_active_playlists_includes_ytlive_with_kind_custom() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let active = get_active_playlists(&pool).await.unwrap();
        let ytlive = active
            .iter()
            .find(|p| p.name == "ytlive")
            .expect("ytlive should exist after ensure_live_playlist_exists");
        assert_eq!(ytlive.kind, "custom");
        assert_eq!(ytlive.current_position, 0);
        assert_eq!(ytlive.ndi_output_name, "SP-live");
    }

    #[tokio::test]
    async fn insert_playlist_defaults_kind_to_youtube() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let created = insert_playlist(&pool, "TestYT", "https://yt.com/test")
            .await
            .unwrap();
        assert_eq!(created.kind, "youtube");
        assert_eq!(created.current_position, 0);
    }

    #[tokio::test]
    async fn append_item_assigns_next_position() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let yt = insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let v1 = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
        let v2 = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;

        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();

        let p1 = append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
        let p2 = append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
        assert_eq!(p1, 0);
        assert_eq!(p2, 1);
    }

    #[tokio::test]
    async fn append_item_duplicate_errors() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let yt = insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let v = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();

        append_playlist_item(&pool, ytlive_id, v).await.unwrap();
        let err = append_playlist_item(&pool, ytlive_id, v).await;
        assert!(err.is_err(), "duplicate append must error");
    }

    #[tokio::test]
    async fn remove_item_compacts_positions() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let yt = insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let v1 = upsert_video(&pool, yt.id, "id1", Some("A"))
            .await
            .unwrap()
            .id;
        let v2 = upsert_video(&pool, yt.id, "id2", Some("B"))
            .await
            .unwrap()
            .id;
        let v3 = upsert_video(&pool, yt.id, "id3", Some("C"))
            .await
            .unwrap()
            .id;
        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();

        append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
        append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
        append_playlist_item(&pool, ytlive_id, v3).await.unwrap();

        remove_playlist_item(&pool, ytlive_id, v2).await.unwrap();

        let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].position, 0);
        assert_eq!(items[0].video_id, v1);
        assert_eq!(items[1].position, 1);
        assert_eq!(items[1].video_id, v3);
    }

    #[tokio::test]
    async fn list_playlist_items_returns_rows_in_position_order() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let yt = insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
        let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();

        append_playlist_item(&pool, ytlive_id, a).await.unwrap();
        append_playlist_item(&pool, ytlive_id, b).await.unwrap();

        let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].video_id, a);
        assert_eq!(items[1].video_id, b);
    }

    #[tokio::test]
    async fn position_for_video_lookup() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        crate::startup::ensure_live_playlist_exists(&pool)
            .await
            .unwrap();
        let yt = insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
        let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();
        append_playlist_item(&pool, ytlive_id, a).await.unwrap();
        append_playlist_item(&pool, ytlive_id, b).await.unwrap();

        let pos = position_for_playlist_item(&pool, ytlive_id, b)
            .await
            .unwrap();
        assert_eq!(pos, Some(1));

        let missing = position_for_playlist_item(&pool, ytlive_id, 999)
            .await
            .unwrap();
        assert_eq!(missing, None);
    }
}
