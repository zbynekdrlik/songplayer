//! Tests (#14) for the karaoke stem-separation queries. Sibling of
//! `models_stems.rs`, wired via `#[path = "models_tests_stems.rs"]`.

#![allow(unused_imports)]

use super::*;
use crate::db;
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

/// Insert a normalized video with an audio sidecar path (stem-eligible).
async fn insert_normalized(pool: &SqlitePool, youtube_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, audio_file_path) \
         VALUES (1, ?, 't', 1, ?, ?) RETURNING id",
    )
    .bind(youtube_id)
    .bind(format!("/c/{youtube_id}_video.mp4"))
    .bind(format!("/c/{youtube_id}_audio.flac"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn backdate(pool: &SqlitePool, id: i64) {
    sqlx::query("UPDATE videos SET stem_next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn selects_normalized_song_with_audio_and_no_stems() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "aaa").await;
    let job = get_next_video_for_stems(&pool).await.unwrap();
    assert_eq!(job.map(|j| j.video_id), Some(id));
}

#[tokio::test]
async fn skips_un_normalized_and_missing_audio_rows() {
    let pool = setup_pool().await;
    // Un-normalized row (no audio sidecar).
    sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title, normalized) VALUES (1, 'nn', 't', 0)")
        .execute(&pool)
        .await
        .unwrap();
    // Normalized but no audio_file_path.
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path) \
         VALUES (1, 'noaudio', 't', 1, '/c/x_video.mp4')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(get_next_video_for_stems(&pool).await.unwrap().is_none());
}

#[tokio::test]
async fn done_and_unsupported_are_excluded() {
    let pool = setup_pool().await;
    let a = insert_normalized(&pool, "done1").await;
    let b = insert_normalized(&pool, "unsup1").await;
    mark_stems_done(&pool, a, "/c/done1_audio_vocals.flac", "/c/done1_audio_instrumental.flac")
        .await
        .unwrap();
    mark_stems_unsupported(&pool, b).await.unwrap();
    assert!(
        get_next_video_for_stems(&pool).await.unwrap().is_none(),
        "done + unsupported rows must not be re-selected"
    );
}

#[tokio::test]
async fn deferral_hides_row_until_backoff_elapses() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "fail1").await;

    let attempts = record_stem_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(
        get_next_video_for_stems(&pool).await.unwrap().is_none(),
        "failed row within backoff is skipped"
    );

    backdate(&pool, id).await;
    assert_eq!(
        get_next_video_for_stems(&pool).await.unwrap().map(|j| j.video_id),
        Some(id),
        "failed row past its backoff is re-selected"
    );
}

#[tokio::test]
async fn deferral_increments_attempts() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "fail2").await;
    let a1 = record_stem_deferral(&pool, id, Duration::from_secs(300)).await.unwrap();
    let a2 = record_stem_deferral(&pool, id, Duration::from_secs(600)).await.unwrap();
    assert_eq!((a1, a2), (1, 2));
}

#[tokio::test]
async fn mark_done_stores_paths_and_resets_backoff() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "ok1").await;
    record_stem_deferral(&pool, id, Duration::from_secs(300)).await.unwrap();
    mark_stems_done(&pool, id, "/c/ok1_audio_vocals.flac", "/c/ok1_audio_instrumental.flac")
        .await
        .unwrap();

    let (v, i, status, attempts): (Option<String>, Option<String>, Option<String>, i64) =
        sqlx::query_as(
            "SELECT vocals_file_path, instrumental_file_path, stem_status, stem_attempts \
             FROM videos WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v.as_deref(), Some("/c/ok1_audio_vocals.flac"));
    assert_eq!(i.as_deref(), Some("/c/ok1_audio_instrumental.flac"));
    assert_eq!(status.as_deref(), Some("done"));
    assert_eq!(attempts, 0, "success resets the retry backoff");
}

#[tokio::test]
async fn oldest_first_selection() {
    let pool = setup_pool().await;
    let first = insert_normalized(&pool, "first").await;
    let _second = insert_normalized(&pool, "second").await;
    assert_eq!(
        get_next_video_for_stems(&pool).await.unwrap().map(|j| j.video_id),
        Some(first),
        "selector is oldest-first by id"
    );
}

#[tokio::test]
async fn count_stems_progress_counts_pending_and_done() {
    let pool = setup_pool().await;
    let a = insert_normalized(&pool, "p1").await;
    let _b = insert_normalized(&pool, "p2").await;
    mark_stems_done(&pool, a, "/c/p1_audio_vocals.flac", "/c/p1_audio_instrumental.flac")
        .await
        .unwrap();
    let (pending, done) = count_stems_progress(&pool).await.unwrap();
    assert_eq!(pending, 1, "one song still needs stems");
    assert_eq!(done, 1, "one song has stems");
}
