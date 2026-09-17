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
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) VALUES (1, 'nn', 't', 0)",
    )
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
    mark_stems_done(
        &pool,
        a,
        "/c/done1_audio_vocals.flac",
        "/c/done1_audio_instrumental.flac",
    )
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
        get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .map(|j| j.video_id),
        Some(id),
        "failed row past its backoff is re-selected"
    );
}

#[tokio::test]
async fn deferral_increments_attempts() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "fail2").await;
    let a1 = record_stem_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    let a2 = record_stem_deferral(&pool, id, Duration::from_secs(600))
        .await
        .unwrap();
    assert_eq!((a1, a2), (1, 2));
}

#[tokio::test]
async fn mark_done_stores_paths_and_resets_backoff() {
    let pool = setup_pool().await;
    let id = insert_normalized(&pool, "ok1").await;
    record_stem_deferral(&pool, id, Duration::from_secs(300))
        .await
        .unwrap();
    mark_stems_done(
        &pool,
        id,
        "/c/ok1_audio_vocals.flac",
        "/c/ok1_audio_instrumental.flac",
    )
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
        get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .map(|j| j.video_id),
        Some(first),
        "selector is oldest-first by id"
    );
}

// ── #177 per-song stems state ────────────────────────────────────────────────

#[test]
fn stems_state_of_maps_every_branch() {
    use StemsState::*;
    // Processing (live in-flight) wins over everything.
    assert_eq!(stems_state_of(None, false, false, true), Processing);
    assert_eq!(stems_state_of(Some("done"), true, true, true), Processing);
    // Both stems on disk → Ready (regardless of the recorded status).
    assert_eq!(stems_state_of(Some("done"), true, true, false), Ready);
    assert_eq!(stems_state_of(None, true, true, false), Ready);
    // One stem missing is NOT ready.
    assert_eq!(stems_state_of(None, true, false, false), Queued);
    assert_eq!(stems_state_of(None, false, true, false), Queued);
    // Terminal unsupported → Unavailable.
    assert_eq!(
        stems_state_of(Some("unsupported"), false, false, false),
        Unavailable
    );
    // Retryable failure → Failed.
    assert_eq!(stems_state_of(Some("failed"), false, false, false), Failed);
    // NULL / unknown status, no stems → Queued.
    assert_eq!(stems_state_of(None, false, false, false), Queued);
    assert_eq!(stems_state_of(Some("weird"), false, false, false), Queued);
}

#[test]
fn stems_state_wire_strings_are_stable() {
    assert_eq!(StemsState::Ready.as_str(), "ready");
    assert_eq!(StemsState::Queued.as_str(), "queued");
    assert_eq!(StemsState::Processing.as_str(), "processing");
    assert_eq!(StemsState::Unavailable.as_str(), "unavailable");
    assert_eq!(StemsState::Failed.as_str(), "failed");
}

#[tokio::test]
async fn queue_position_is_one_based_oldest_first() {
    let pool = setup_pool().await;
    let a = insert_normalized(&pool, "q1").await;
    let b = insert_normalized(&pool, "q2").await;
    let c = insert_normalized(&pool, "q3").await;
    assert_eq!(queue_position(&pool, a).await.unwrap(), Some(1));
    assert_eq!(queue_position(&pool, b).await.unwrap(), Some(2));
    assert_eq!(queue_position(&pool, c).await.unwrap(), Some(3));
}

#[tokio::test]
async fn queue_position_none_for_ineligible_rows() {
    let pool = setup_pool().await;
    let done = insert_normalized(&pool, "posd").await;
    let unsup = insert_normalized(&pool, "posu").await;
    let failed = insert_normalized(&pool, "posf").await;
    let queued = insert_normalized(&pool, "posq").await;
    mark_stems_done(&pool, done, "/c/posd_v.flac", "/c/posd_i.flac")
        .await
        .unwrap();
    mark_stems_unsupported(&pool, unsup).await.unwrap();
    // failed within backoff → not eligible → no position.
    record_stem_deferral(&pool, failed, Duration::from_secs(600))
        .await
        .unwrap();
    assert_eq!(queue_position(&pool, done).await.unwrap(), None);
    assert_eq!(queue_position(&pool, unsup).await.unwrap(), None);
    assert_eq!(queue_position(&pool, failed).await.unwrap(), None);
    // The only still-eligible row is `queued`; ineligible older rows do NOT
    // count toward its position.
    assert_eq!(queue_position(&pool, queued).await.unwrap(), Some(1));
    // A failed row PAST its backoff becomes eligible again and gets a position.
    backdate(&pool, failed).await;
    assert_eq!(queue_position(&pool, failed).await.unwrap(), Some(1));
    assert_eq!(queue_position(&pool, queued).await.unwrap(), Some(2));
}

#[tokio::test]
async fn video_stems_info_resolves_title_and_state() {
    let pool = setup_pool().await;
    let ready = insert_normalized(&pool, "vsi_ready").await;
    let queued = insert_normalized(&pool, "vsi_q").await;
    mark_stems_done(&pool, ready, "/c/r_v.flac", "/c/r_i.flac")
        .await
        .unwrap();

    let info = video_stems_info(&pool, ready, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.state, StemsState::Ready);
    assert_eq!(info.title, "t"); // fixture title (no song set)
    assert_eq!(info.attempts, 0);

    let info = video_stems_info(&pool, queued, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.state, StemsState::Queued);

    // Live in-flight overrides to Processing.
    let info = video_stems_info(&pool, queued, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.state, StemsState::Processing);

    assert!(
        video_stems_info(&pool, 99999, false)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stems_state_map_marks_each_row() {
    let pool = setup_pool().await;
    let ready = insert_normalized(&pool, "m_ready").await;
    let queued = insert_normalized(&pool, "m_q").await;
    let unsup = insert_normalized(&pool, "m_u").await;
    mark_stems_done(&pool, ready, "/c/mr_v.flac", "/c/mr_i.flac")
        .await
        .unwrap();
    mark_stems_unsupported(&pool, unsup).await.unwrap();

    let map = stems_state_map(&pool, 1, Some(queued)).await.unwrap();
    assert_eq!(map.get(&ready).map(String::as_str), Some("ready"));
    // `queued` is the live in-flight id here → Processing.
    assert_eq!(map.get(&queued).map(String::as_str), Some("processing"));
    assert_eq!(map.get(&unsup).map(String::as_str), Some("unavailable"));

    // No in-flight id → the pending row is plain queued.
    let map = stems_state_map(&pool, 1, None).await.unwrap();
    assert_eq!(map.get(&queued).map(String::as_str), Some("queued"));
}

#[tokio::test]
async fn enqueue_stems_reopens_failed_and_unsupported() {
    let pool = setup_pool().await;
    let failed = insert_normalized(&pool, "eq_f").await;
    let unsup = insert_normalized(&pool, "eq_u").await;
    record_stem_deferral(&pool, failed, Duration::from_secs(3600))
        .await
        .unwrap();
    mark_stems_unsupported(&pool, unsup).await.unwrap();
    // Both currently ineligible for selection.
    assert!(get_next_video_for_stems(&pool).await.unwrap().is_none());

    enqueue_stems(&pool, failed).await.unwrap();
    enqueue_stems(&pool, unsup).await.unwrap();

    // The oldest re-enqueued row (failed) is now immediately selectable.
    assert_eq!(
        get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .map(|j| j.video_id),
        Some(failed),
    );
    // And both carry a queue position again.
    assert_eq!(queue_position(&pool, failed).await.unwrap(), Some(1));
    assert_eq!(queue_position(&pool, unsup).await.unwrap(), Some(2));
}

#[tokio::test]
async fn count_stems_progress_counts_pending_and_done() {
    let pool = setup_pool().await;
    let a = insert_normalized(&pool, "p1").await;
    let _b = insert_normalized(&pool, "p2").await;
    mark_stems_done(
        &pool,
        a,
        "/c/p1_audio_vocals.flac",
        "/c/p1_audio_instrumental.flac",
    )
    .await
    .unwrap();
    let (pending, done) = count_stems_progress(&pool).await.unwrap();
    assert_eq!(pending, 1, "one song still needs stems");
    assert_eq!(done, 1, "one song has stems");
}
