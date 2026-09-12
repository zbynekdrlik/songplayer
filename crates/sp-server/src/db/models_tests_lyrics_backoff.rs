//! RED tests (#144) for the durable lyrics retry backoff.
//!
//! Mirrors the downloader's per-row backoff (#140): a lyrics row the worker
//! could not process (missing AAI key, vocal isolation failed, transient ASR
//! error) is deferred with `record_lyrics_deferral` so the selector skips it
//! until `lyrics_next_attempt_at`, instead of re-picking it every 5 s tick
//! (37-min hot-loop observed on `3_ccqgwVZYM`). A success / terminal stamp
//! resets the backoff columns.
//!
//! Sibling file wired from `models.rs` via
//! `#[path = "models_tests_lyrics_backoff.rs"] #[cfg(test)]
//! mod tests_lyrics_backoff;` to honor the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;
use crate::db;
use crate::lyrics::LYRICS_PIPELINE_VERSION;
use crate::lyrics::reprocess::get_next_video_for_lyrics;
use std::time::Duration;

async fn setup_pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

async fn insert_video(pool: &SqlitePool, youtube_id: &str, has_lyrics: i64, version: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_pipeline_version, lyrics_manual_priority) \
         VALUES (1, ?, 1, ?, ?, 0) RETURNING id",
    )
    .bind(youtube_id)
    .bind(has_lyrics)
    .bind(version)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_next_attempt_past(pool: &SqlitePool, id: i64) {
    sqlx::query(
        "UPDATE videos SET lyrics_next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn deferral_removes_manual_row_from_selector_until_due() {
    let pool = setup_pool().await;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, lyrics_manual_priority) \
         VALUES (1, 'manual1', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Before deferral: selected.
    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(id), "manual row must be selected");

    // Defer for 5 min → excluded.
    let attempts = record_lyrics_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(attempts, 1, "first deferral returns attempt count 1");
    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert!(
        row.is_none(),
        "deferred row must NOT be selected before due"
    );

    // Backdate next_attempt_at → eligible again.
    set_next_attempt_past(&pool, id);
    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert_eq!(
        row.map(|r| r.id),
        Some(id),
        "row past its backoff must be selected again"
    );
}

#[tokio::test]
async fn record_lyrics_deferral_increments_attempts() {
    let pool = setup_pool().await;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, lyrics_manual_priority) \
         VALUES (1, 'm', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let a1 = record_lyrics_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    let a2 = record_lyrics_deferral(&pool, id, Duration::from_secs(600))
        .await
        .unwrap();
    assert_eq!((a1, a2), (1, 2), "consecutive deferrals return 1 then 2");
}

#[tokio::test]
async fn success_stamp_resets_backoff_columns() {
    let pool = setup_pool().await;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, lyrics_manual_priority) \
         VALUES (1, 'm', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    record_lyrics_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    mark_video_lyrics_complete(&pool, id, "yt_subs", LYRICS_PIPELINE_VERSION, None, None)
        .await
        .unwrap();

    let attempts: i64 = sqlx::query_scalar("SELECT lyrics_attempts FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let next_attempt_at: Option<String> =
        sqlx::query_scalar("SELECT lyrics_next_attempt_at FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0, "success resets lyrics_attempts to 0");
    assert!(
        next_attempt_at.is_none(),
        "success resets lyrics_next_attempt_at to NULL"
    );
}

#[tokio::test]
async fn null_bucket_honours_backoff() {
    let pool = setup_pool().await;
    let id = insert_video(&pool, "null1", 0, 0).await;

    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(id), "null-bucket row is selected");

    record_lyrics_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert!(row.is_none(), "deferred null-bucket row must be skipped");
}

#[tokio::test]
async fn stale_bucket_honours_backoff() {
    let pool = setup_pool().await;
    // has_lyrics=1 at an OLD pipeline version → stale bucket.
    let id = insert_video(&pool, "stale1", 1, 1).await;

    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert_eq!(row.map(|r| r.id), Some(id), "stale-bucket row is selected");

    record_lyrics_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    let row = get_next_video_for_lyrics(&pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap();
    assert!(row.is_none(), "deferred stale-bucket row must be skipped");
}
