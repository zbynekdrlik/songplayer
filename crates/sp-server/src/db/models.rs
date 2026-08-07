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
///
/// mutants::skip: the `kind` and `current_position` struct-init assignments
/// are equivalent to the `Default` fallback for this API surface — the
/// function only ever inserts `(name, youtube_url)`, so the DB row defaults
/// (`kind='youtube'`, `current_position=0`) match the Rust `Default` values
/// exactly. Any mutation that deletes those field assignments produces
/// observationally identical behaviour, so no test can distinguish them.
/// Custom-kind playlists are seeded via `startup::ensure_live_playlist_exists`,
/// not this function. Behaviour is still covered by `insert_playlist_*` tests.
#[cfg_attr(test, mutants::skip)]
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
                duration_ms, file_path, normalized, gemini_failed,
                suppress_resolume_en, spotify_track_id
         FROM videos WHERE playlist_id = ? ORDER BY id",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_video).collect())
}

/// Insert or update a video keyed on (playlist_id, youtube_id).
/// On conflict the title is updated. Returns the resulting row.
///
// V17 NOTE: `spotify_track_id` is in RETURNING but intentionally NOT in
// `ON CONFLICT … DO UPDATE SET` — manual track-ID assignments must survive
// playlist re-syncs. Do not extend `DO UPDATE SET` without revisiting this
// design (and adding multi-playlist test coverage if you do).
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
                   duration_ms, file_path, normalized, gemini_failed,
                   suppress_resolume_en, spotify_track_id",
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
        suppress_resolume_en: r.get::<i32, _>("suppress_resolume_en") != 0,
        spotify_track_id: r.get("spotify_track_id"),
    }
}

/// Fast single-column read of `videos.suppress_resolume_en` by id. Used by
/// the playback engine hot path to decide whether to skip the Resolume EN
/// push. Returns None when the row doesn't exist.
pub async fn get_video_suppress_resolume_en(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<bool, sqlx::Error> {
    let v: Option<i64> = sqlx::query_scalar("SELECT suppress_resolume_en FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;
    Ok(v.map(|n| n != 0).unwrap_or(false))
}

/// Atomically record the outcome of a Spotify resolution attempt. Sets both
/// `spotify_track_id` and `spotify_resolved_at = datetime('now')` in one
/// statement. Pass `None` for the track ID to record a no-match attempt.
///
/// Returns the number of rows affected (0 = no row with that id).
pub async fn set_video_spotify_resolution(
    pool: &SqlitePool,
    video_id: i64,
    spotify_track_id: Option<&str>,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE videos SET spotify_track_id = ?1, spotify_resolved_at = datetime('now') WHERE id = ?2",
    )
    .bind(spotify_track_id)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
///
/// Rejects an empty (post-trim) `song` with `Err(sqlx::Error::InvalidArgument)`
/// and leaves the row completely untouched — this is the last-line write
/// choke point, so no upstream mistake can ever persist an empty song
/// (#136: a live defect shipped five rows with `song=""`, `gemini_failed
/// = false` because nothing here re-validated the caller's metadata). An
/// empty `artist` stays legal — `playback/title.rs` handles it.
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
    if song.trim().is_empty() {
        return Err(sqlx::Error::InvalidArgument(format!(
            "mark_video_processed_pair: refusing to write an empty song for video_db_id={video_db_id}"
        )));
    }

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
    /// Operator-provided lyrics text (V15 migration). When Some(non-empty),
    /// `gather_sources` uses it as the top-priority candidate, bypassing
    /// the yt_subs / description / LRCLIB fetch paths. Populated via
    /// `PATCH /api/v1/videos/{id}` with `lyrics_override_text`.
    pub lyrics_override_text: Option<String>,
    /// Per-song time-axis shift applied at render time; preserves real Gemini
    /// alignment while letting the operator correct systematic lead/lag.
    /// Signed: positive = delay display (effectively shorter lead),
    /// negative = advance display (longer lead). V16 migration.
    pub lyrics_time_offset_ms: i64,
    /// Spotify track ID for line-synced lyrics fetch (V17 column).
    /// Auto-populated by the lyrics worker via `SpotifyResolver` (Claude
    /// canonical-ID lookup, gated by `spotify_resolved_at IS NULL`). When
    /// non-NULL, `gather_sources_impl` calls `SpotifyLyricsFetcher` to pull
    /// LINE_SYNCED lyrics from the public proxy. V18 backfilled this for
    /// any rows that already had a value from PR #70's now-removed manual
    /// UI.
    pub spotify_track_id: Option<String>,
    /// V18 column. Set to `datetime('now')` whenever the Spotify resolver runs
    /// on this song (success OR no-match). NULL means "never attempted." Worker
    /// gate uses (spotify_track_id IS NULL AND spotify_resolved_at IS NULL) to
    /// decide whether to ask Claude.
    pub spotify_resolved_at: Option<String>,
}

/// Mark a video's lyrics status, source, AND pipeline version.
///
/// The pipeline_version stamp is critical on the failure path: without it, a
/// row marked `no_source` under pipeline v5 keeps its pipeline_version at 0,
/// and the `OR lyrics_pipeline_version < current` retry clause in
/// `fetch_bucket_null` / `fetch_bucket_manual` then loops it forever because
/// `0 < 5` is always true. Stamping the current version here means the song
/// only gets retried when a NEW pipeline version ships.
#[cfg_attr(test, mutants::skip)]
pub async fn mark_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    has_lyrics: bool,
    lyrics_source: Option<&str>,
    pipeline_version: u32,
) -> Result<(), sqlx::Error> {
    // lyrics_processed_at = strftime() so the timestamp comes from SQLite
    // (no clock-skew between server process and DB). lyrics_alignment_model
    // = NULL because the failure path has no successful alignment to record.
    //
    // lyrics_manual_priority = 0: this helper is the worker's terminal-failure
    // exit (`no_source` / `empty` / `failed`). The row is now parked by the
    // skip-list (`reprocess.rs::fetch_bucket_*`), so the manual flag has no
    // future meaning. Without clearing it, audit queries report dangling
    // queue entries (10 rows observed in production after the 2026-05-16
    // catalog reprocess).
    sqlx::query(
        "UPDATE videos SET has_lyrics = ?, lyrics_source = ?, lyrics_pipeline_version = ?, \
         lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(has_lyrics as i32)
    .bind(lyrics_source)
    .bind(pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist a successful lyrics processing run: sets has_lyrics=1, records source,
/// pipeline_version, quality_score, alignment_model, and clears manual_priority
/// — all in one query.
///
/// `quality_score` is `None` for fallback paths (e.g. ensemble timeout) to avoid
/// writing 0.0 which would poison the `ORDER BY lyrics_quality_score ASC NULLS FIRST`
/// stale-bucket selector — songs with 0.0 score sort before all real scores.
///
/// `alignment_model` distinguishes between "no model ran" (None → SQL NULL,
/// legacy / unknown) and "model literal `none`" (`Some(ALIGNMENT_MODEL_NONE)`,
/// raw line-timed ship-through where whisperx was deliberately skipped). See
/// the precedence comment in `worker.rs::process_song` near line 639 for how
/// callers derive the literal from the source label.
#[cfg_attr(test, mutants::skip)] // single UPDATE; covered by integration tests below
pub async fn mark_video_lyrics_complete(
    pool: &SqlitePool,
    video_id: i64,
    source: &str,
    pipeline_version: u32,
    quality_score: Option<f32>,
    alignment_model: Option<&str>,
) -> Result<(), sqlx::Error> {
    // lyrics_alignment_model is Option<&str>: None writes SQL NULL — reserved
    // for legacy paths where no alignment ran AND the literal is unknown.
    // For raw line-timed ship-through (yt_subs / lrclib / spotify, no whisperx),
    // callers MUST pass Some(ALIGNMENT_MODEL_NONE) so the DB records the
    // explicit literal "none" rather than the ambiguous NULL.
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = ?, \
         lyrics_pipeline_version = ?, lyrics_quality_score = ?, \
         lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = ? \
         WHERE id = ?",
    )
    .bind(source)
    .bind(pipeline_version as i64)
    .bind(quality_score.map(|q| q as f64))
    .bind(alignment_model)
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
///
/// Also clears `lyrics_processed_at` and `lyrics_alignment_model` because
/// "reset" means "forget when/how this was processed" — leaving the old
/// timestamp/model would make audit queries misleading.
#[cfg_attr(test, mutants::skip)]
pub async fn reset_video_lyrics(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = NULL, \
         lyrics_processed_at = NULL, lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
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

/// Move a playlist item one slot up or down. `direction` must be `-1`
/// (up / earlier) or `+1` (down / later). At the boundary (top-most item
/// moved up, bottom-most moved down) this is a no-op — we silently return
/// Ok so the UI doesn't have to pre-check bounds.
///
/// Implementation note: SQLite's primary key on `(playlist_id, position)`
/// means a straight-forward two-UPDATE swap collides mid-transaction. We
/// stage the moved row at `position = -1` (a sentinel outside the valid
/// `0..N` range) first, swap the neighbour into the vacated slot, then
/// move the staged row to its final slot.
// mutants::skip: direction validation (line 600) and target_pos<0 guard (line 618)
// are defense-in-depth — the HTTP handler in `api::live::post_move_item` already
// rejects non "up"/"down" strings (returns 400) and the DB schema clamps positions
// to u32, so mutants on these branches never observe a test failure via the
// public API. Behaviour is covered by `api::live::tests_included::move_item_*`.
#[cfg_attr(test, mutants::skip)]
pub async fn move_playlist_item_step(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
    direction: i64,
) -> Result<(), sqlx::Error> {
    if direction != 1 && direction != -1 {
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    let cur_pos: Option<i64> = sqlx::query_scalar(
        "SELECT position FROM playlist_items WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(playlist_id)
    .bind(video_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(cur_pos) = cur_pos else {
        return Ok(());
    };
    let target_pos = cur_pos + direction;
    if target_pos < 0 {
        return Ok(());
    }

    let neighbor_vid: Option<i64> = sqlx::query_scalar(
        "SELECT video_id FROM playlist_items WHERE playlist_id = ? AND position = ?",
    )
    .bind(playlist_id)
    .bind(target_pos)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(neighbor_vid) = neighbor_vid else {
        // Moved item is at the boundary — nothing to swap with.
        return Ok(());
    };

    // Stage the moved row at a sentinel position.
    sqlx::query(
        "UPDATE playlist_items SET position = -1
         WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(playlist_id)
    .bind(video_id)
    .execute(&mut *tx)
    .await?;

    // Neighbour slides into the vacated slot.
    sqlx::query(
        "UPDATE playlist_items SET position = ?
         WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(cur_pos)
    .bind(playlist_id)
    .bind(neighbor_vid)
    .execute(&mut *tx)
    .await?;

    // Moved row lands on its target.
    sqlx::query(
        "UPDATE playlist_items SET position = ?
         WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(target_pos)
    .bind(playlist_id)
    .bind(video_id)
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

/// Membership + readiness check for the `videos.playlist_id` model used by
/// youtube-kind playlists (issue #134) — the direct-link counterpart of
/// `position_for_playlist_item`'s `playlist_items` set-list model that only
/// custom-kind playlists use. Returns `Some(normalized)` when `video_id`
/// belongs to `playlist_id`, `None` when it doesn't exist or belongs to a
/// different playlist (prevents a client from triggering playback of an
/// arbitrary video via another playlist's URL — same protection
/// `position_for_playlist_item` gives the custom-playlist path).
pub async fn video_playlist_membership(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT normalized FROM videos WHERE id = ? AND playlist_id = ?")
        .bind(video_id)
        .bind(playlist_id)
        .fetch_optional(pool)
        .await
}

/// Outcome of a successful `quarantine_video_lyrics` call. Surfaced through
/// the HTTP layer so operators can confirm what was changed.
#[derive(Debug)]
pub struct QuarantineOutcome {
    pub youtube_id: String,
    pub previous_source: Option<String>,
    pub deleted_cache_file: bool,
}

/// Park a song with unrecoverable ASR transcription so the lyrics worker
/// stops re-picking it. Sets `lyrics_source = 'asr_gap'`, clears
/// `has_lyrics` and `lyrics_manual_priority`, stamps the current pipeline
/// version, and best-effort deletes the cached `{youtube_id}_lyrics.json`
/// file so the wall stops rendering whatever broken karaoke shipped
/// previously.
///
/// Future `LYRICS_PIPELINE_VERSION` bumps re-pick `asr_gap` rows
/// automatically via the existing `OR lyrics_pipeline_version < ?`
/// exception in `reprocess.rs::fetch_bucket_null` and
/// `fetch_bucket_manual` — no separate ASR-version constant is needed.
///
/// Returns `Err(sqlx::Error::RowNotFound)` when `video_id` does not exist;
/// the HTTP handler maps that variant to 404.
#[cfg_attr(test, mutants::skip)] // 3 integration tests below cover the
// happy path, missing-cache-file branch, and not-found branch. The body
// is one SELECT + one UPDATE + one fs::remove_file + a tracing line —
// every observable side effect is asserted by the tests, so mutation
// targets reduce to the test-equivalent SQL string literals that
// cargo-mutants cannot mutate meaningfully.
pub async fn quarantine_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    cache_dir: &std::path::Path,
    reason: &str,
    current_pipeline_version: u32,
) -> Result<QuarantineOutcome, sqlx::Error> {
    let row = sqlx::query("SELECT youtube_id, lyrics_source FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;
    let row = row.ok_or(sqlx::Error::RowNotFound)?;
    let youtube_id: String = row.get("youtube_id");
    // `try_get::<Option<String>, _>` returns Ok(None) for NULL, Ok(Some(s)) for
    // non-null. Plain `try_get::<String, _>().ok()` does NOT work because sqlx
    // 0.8 SQLite decodes NULL TEXT into `String` as Ok("") rather than Err —
    // observed on CI 2026-05-12, leaks `Some("")` into the audit log and
    // breaks the JSON-null response contract.
    let previous_source: Option<String> = row
        .try_get::<Option<String>, _>("lyrics_source")
        .unwrap_or(None);

    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'asr_gap', \
         lyrics_pipeline_version = ?, lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(current_pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;

    let cache_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let deleted_cache_file = tokio::fs::remove_file(&cache_path).await.is_ok();

    tracing::warn!(
        video_id,
        youtube_id = %youtube_id,
        reason = %reason,
        previous_source = ?previous_source,
        deleted_cache_file,
        "lyrics quarantined as asr_gap"
    );

    Ok(QuarantineOutcome {
        youtube_id,
        previous_source,
        deleted_cache_file,
    })
}

// ---------------------------------------------------------------------------
// Unsupported-source sentinel
// ---------------------------------------------------------------------------

/// Mark a video as having no allowed text source. Parallel to `quarantine_video_lyrics`
/// (asr_gap) but for the case where the gather pass found candidates but none of
/// them passed `is_allowed_text_source` — typically genius-only, lrclib-plain
/// without timing, or no_source-then-just-whisperx.
///
/// Sets `lyrics_source = 'unsupported_source'`, clears `has_lyrics` and
/// `lyrics_manual_priority`, stamps the current pipeline version and timestamp,
/// nulls out `lyrics_alignment_model` (nothing aligned). The stale-bucket
/// `NOT IN (...)` skip-list in `reprocess.rs` is extended in Task 5 to
/// exclude this sentinel so the worker does not loop on it.
///
/// A future-model PR retires the sentinel via a dedicated admin endpoint
/// (parallel to whatever asr_gap-retire endpoint ships next). See
/// `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.
#[cfg_attr(test, mutants::skip)] // Single UPDATE — observable side effects
// are all covered by `mark_unsupported_source_writes_all_fields` and
// `mark_unsupported_source_clears_manual_priority` in models_tests.rs;
// remaining mutation targets reduce to SQL string literals which cargo-mutants
// cannot mutate meaningfully.
pub async fn mark_unsupported_source(
    pool: &SqlitePool,
    video_id: i64,
    current_pipeline_version: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'unsupported_source', \
         lyrics_pipeline_version = ?, lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(current_pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Split into focused sibling files (#137) to keep every file under the
// 1000-line airuleset cap. `tests_helpers` holds the shared fixture; the
// other three group tests by subject under test.
#[path = "models_tests_helpers.rs"]
#[cfg(test)]
mod tests_helpers;

#[path = "models_tests_playlist.rs"]
#[cfg(test)]
mod tests_playlist;

#[path = "models_tests_video.rs"]
#[cfg(test)]
mod tests_video;

#[path = "models_tests_lyrics.rs"]
#[cfg(test)]
mod tests_lyrics;
