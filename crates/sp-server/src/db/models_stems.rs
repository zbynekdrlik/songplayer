//! Karaoke stem-separation queries (#14) — split out of `models.rs` to keep
//! it under the 1000-line airuleset cap. Re-exported via
//! `pub use models_stems::*;` so call sites use `crate::db::models::…`.
//!
//! Mirrors the V22 lyrics retry-backoff bookkeeping: the stem worker selects the
//! next normalized song that has no stems yet (or whose last attempt failed and
//! whose backoff has elapsed), then records the two sidecar paths + a terminal
//! status. `stem_status` values: NULL = pending, 'done', 'failed' (retryable,
//! backoff-gated), 'unsupported' (terminal — e.g. audio too long / no vocals).

use sqlx::{Row, SqlitePool};

/// One unit of stem-separation work: a normalized song whose `{id}_audio.flac`
/// exists but whose stems are not yet generated.
#[derive(Debug, Clone, PartialEq)]
pub struct StemJob {
    pub video_id: i64,
    pub youtube_id: String,
    pub audio_file_path: String,
    pub duration_ms: Option<i64>,
    pub song: Option<String>,
    pub artist: Option<String>,
}

/// Select the next song needing stem separation, oldest-first. Eligible rows are
/// normalized, have an `audio_file_path`, are not already `done`/`unsupported`,
/// and (if previously `failed`) have passed their backoff window. Returns `None`
/// when nothing is due.
pub async fn get_next_video_for_stems(
    pool: &SqlitePool,
) -> Result<Option<StemJob>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, youtube_id, audio_file_path, duration_ms, song, artist \
         FROM videos \
         WHERE normalized = 1 \
           AND audio_file_path IS NOT NULL \
           AND (stem_status IS NULL OR stem_status = 'failed') \
           AND (stem_next_attempt_at IS NULL \
                OR stem_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
         ORDER BY id ASC \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| StemJob {
        video_id: r.get("id"),
        youtube_id: r.get("youtube_id"),
        audio_file_path: r.get("audio_file_path"),
        duration_ms: r.get("duration_ms"),
        song: r.get("song"),
        artist: r.get("artist"),
    }))
}

/// Record a successful separation: store both sidecar paths, mark `done`, and
/// reset the retry backoff.
pub async fn mark_stems_done(
    pool: &SqlitePool,
    video_id: i64,
    vocals_path: &str,
    instrumental_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET vocals_file_path = ?, instrumental_file_path = ?, \
             stem_status = 'done', stem_attempts = 0, stem_next_attempt_at = NULL \
         WHERE id = ?",
    )
    .bind(vocals_path)
    .bind(instrumental_path)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a transient separation failure: mark `failed`, increment
/// `stem_attempts`, and schedule `stem_next_attempt_at = now + backoff` (same
/// `strftime` format the selector compares against). Returns the new attempt
/// count. The song is retried once the backoff elapses.
pub async fn record_stem_deferral(
    pool: &SqlitePool,
    video_id: i64,
    backoff: std::time::Duration,
) -> Result<u32, sqlx::Error> {
    let secs = backoff.as_secs() as i64;
    let current: i64 = sqlx::query_scalar("SELECT stem_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(pool)
        .await?;
    let new_attempts = current + 1;
    sqlx::query(
        "UPDATE videos \
         SET stem_status = 'failed', stem_attempts = ?, \
             stem_next_attempt_at = \
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) \
         WHERE id = ?",
    )
    .bind(new_attempts)
    .bind(secs)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(new_attempts as u32)
}

/// Mark a song as terminally unsupported for stem separation (no retry).
pub async fn mark_stems_unsupported(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET stem_status = 'unsupported', stem_next_attempt_at = NULL \
         WHERE id = ?",
    )
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `(pending, done)` stem counts for the dashboard badge. `pending` counts every
/// normalized song with an audio sidecar that is not yet `done`/`unsupported`
/// (NULL or `failed`); `done` counts songs with both stems written.
pub async fn count_stems_progress(pool: &SqlitePool) -> Result<(i64, i64), sqlx::Error> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos \
         WHERE normalized = 1 AND audio_file_path IS NOT NULL \
           AND (stem_status IS NULL OR stem_status = 'failed')",
    )
    .fetch_one(pool)
    .await?;
    let done: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos WHERE stem_status = 'done'",
    )
    .fetch_one(pool)
    .await?;
    Ok((pending, done))
}

#[cfg(test)]
#[path = "models_tests_stems.rs"]
mod tests;
