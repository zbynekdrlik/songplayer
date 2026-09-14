//! Per-row lyrics retry backoff (#144) — split out of `models.rs` to keep
//! that file under the 1000-line airuleset cap. Re-exported from `models.rs`
//! via `pub use models_lyrics_backoff::*;` so call sites use
//! `crate::db::models::record_lyrics_deferral`.
//!
//! Mirrors the downloader's `record_download_failure` (#140): a lyrics row the
//! worker could not process this pass is deferred with an exponential backoff
//! (`downloader::retry_backoff`) so the selector skips it until due instead of
//! re-picking it every 5 s tick. Success / terminal stamps
//! (`mark_video_lyrics{,_complete}`, `mark_unsupported_source`) reset the
//! columns back to (0, NULL).

use sqlx::SqlitePool;

/// Record a deferral for `video_id`: increment `lyrics_attempts` and schedule
/// `lyrics_next_attempt_at = now + backoff`. Returns the new attempt count.
///
/// The timestamp is written with SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ')` —
/// the SAME format the reprocess bucket queries compare against — so the
/// string comparison is lexically correct (chrono's `to_rfc3339` `+00:00`
/// offset would not sort against strftime's `Z`). The backoff itself is
/// computed by the caller via `downloader::retry_backoff` so the math is not
/// duplicated.
pub async fn record_lyrics_deferral(
    pool: &SqlitePool,
    video_id: i64,
    backoff: std::time::Duration,
) -> Result<u32, sqlx::Error> {
    let secs = backoff.as_secs() as i64;
    let current: i64 = sqlx::query_scalar("SELECT lyrics_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(pool)
        .await?;
    let new_attempts = current + 1;
    sqlx::query(
        "UPDATE videos \
         SET lyrics_attempts = ?, \
             lyrics_next_attempt_at = \
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
