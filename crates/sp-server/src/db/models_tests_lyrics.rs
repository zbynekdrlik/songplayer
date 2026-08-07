//! Lyrics-state tests for `db::models` — `mark_video_lyrics`/`_complete`
//! writes, quarantine sentinel handling, `mark_unsupported_source`, and
//! `reset_video_lyrics`. Split out of the former monolithic
//! `models_tests.rs` (#137) to keep every file under the 1000-line
//! airuleset cap. Included as a sibling file via
//! `#[path = "models_tests_lyrics.rs"] #[cfg(test)] mod tests_lyrics;`
//! from `models.rs`.

#![allow(unused_imports)]

use super::tests_helpers::setup_with_video;
use super::*;
use crate::db;

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

    mark_video_lyrics_complete(&pool, 1, "ensemble:qwen3+autosub", 2, Some(0.85), None)
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

    mark_video_lyrics_complete(&pool, 1, "yt_subs", 2, None, None)
        .await
        .unwrap();

    let q: Option<f64> = sqlx::query_scalar("SELECT lyrics_quality_score FROM videos WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        q, None,
        "fallback path must write NULL, not 0.0 — 0.0 poisons the NULLS FIRST queue ordering"
    );
}

#[tokio::test]
async fn mark_video_lyrics_stamps_pipeline_version_on_failure() {
    // Regression guard for the infinite-loop production bug: a song that fails
    // processing must record the current pipeline version, otherwise the null
    // bucket's `OR lyrics_pipeline_version < current` retry clause brings it
    // back every poll because the default version (0) is always < current.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (99, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
                 VALUES (1, 99, 'loop_me', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    mark_video_lyrics(&pool, 1, false, Some("no_source"), 5)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version FROM videos WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("has_lyrics"), 0);
    assert_eq!(row.get::<String, _>("lyrics_source"), "no_source");
    assert_eq!(
        row.get::<i64, _>("lyrics_pipeline_version"),
        5,
        "failure write MUST stamp current pipeline version or the retry \
         filter will loop the song forever (0 < current is always true)"
    );
}

#[tokio::test]
async fn quarantine_video_lyrics_sets_sentinel_and_deletes_cache_file() {
    let (pool, id) = setup_with_video().await;
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = 'ensemble:gemini', \
         lyrics_pipeline_version = 5, lyrics_manual_priority = 1 WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path();
    let cache_file = cache_dir.join("yt123_lyrics.json");
    tokio::fs::write(&cache_file, b"{\"version\":5,\"lines\":[]}")
        .await
        .unwrap();

    let outcome = quarantine_video_lyrics(&pool, id, cache_dir, "ASR missed bridge", 20)
        .await
        .unwrap();

    assert_eq!(outcome.youtube_id, "yt123");
    assert_eq!(outcome.previous_source.as_deref(), Some("ensemble:gemini"));
    assert!(outcome.deleted_cache_file);
    assert!(
        !cache_file.exists(),
        "cache file must be deleted so the wall stops showing broken karaoke"
    );

    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_manual_priority \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("has_lyrics"), 0);
    assert_eq!(row.get::<String, _>("lyrics_source"), "asr_gap");
    assert_eq!(row.get::<i64, _>("lyrics_pipeline_version"), 20);
    assert_eq!(row.get::<i64, _>("lyrics_manual_priority"), 0);
}

#[tokio::test]
async fn quarantine_video_lyrics_handles_missing_cache_file() {
    let (pool, id) = setup_with_video().await;
    let tmp = tempfile::tempdir().unwrap();
    let outcome = quarantine_video_lyrics(&pool, id, tmp.path(), "", 20)
        .await
        .unwrap();

    assert!(!outcome.deleted_cache_file);
    assert!(
        outcome.previous_source.is_none(),
        "previous_source must be None when row had NULL lyrics_source"
    );
    let source: String = sqlx::query_scalar("SELECT lyrics_source FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source, "asr_gap");
}

#[tokio::test]
async fn quarantine_video_lyrics_returns_not_found_for_missing_id() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let err = quarantine_video_lyrics(&pool, 999, tmp.path(), "", 20)
        .await
        .expect_err("expected NotFound for missing video_id");
    assert!(
        matches!(err, sqlx::Error::RowNotFound),
        "must surface RowNotFound so the handler can return 404; got {err:?}"
    );
}

#[tokio::test]
async fn mark_video_lyrics_writes_processed_at_and_null_model_on_failure() {
    let (pool, video_id) = setup_with_video().await;
    // Seed manual_priority=1 so the assertion below proves the failure path
    // clears it (regression for the 2026-05-16 10-row dangling-flag bug).
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(video_id)
        .execute(&pool)
        .await
        .unwrap();
    mark_video_lyrics(&pool, video_id, false, Some("failed"), 20)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT lyrics_processed_at, lyrics_alignment_model, lyrics_manual_priority \
         FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    let priority: i64 = row.get("lyrics_manual_priority");
    assert!(
        processed_at.is_some(),
        "processed_at must be set on failure path"
    );
    assert!(
        model.is_none(),
        "alignment_model must be NULL on failure path"
    );
    assert_eq!(
        priority, 0,
        "manual_priority must be cleared on failure path (regression for dangling flag bug)"
    );
}

#[tokio::test]
async fn mark_video_lyrics_complete_writes_processed_at_and_explicit_model() {
    let (pool, video_id) = setup_with_video().await;
    mark_video_lyrics_complete(
        &pool,
        video_id,
        "description+whisperx-large-v3@rev1",
        20,
        Some(0.85),
        Some(crate::lyrics::ALIGNMENT_MODEL_WHISPERX_V3_REV1),
    )
    .await
    .unwrap();
    let row =
        sqlx::query("SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(
        processed_at.is_some(),
        "processed_at must be set on success path"
    );
    assert_eq!(
        model.as_deref(),
        Some("whisperx-large-v3@rev1"),
        "alignment_model must round-trip the literal"
    );
}

#[tokio::test]
async fn reset_video_lyrics_clears_processed_at_and_model() {
    let (pool, video_id) = setup_with_video().await;
    // Seed with non-NULL values first.
    mark_video_lyrics_complete(
        &pool,
        video_id,
        "yt_subs",
        20,
        Some(0.9),
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE),
    )
    .await
    .unwrap();
    // Now reset.
    reset_video_lyrics(&pool, video_id).await.unwrap();
    let row =
        sqlx::query("SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(
        processed_at.is_none() && model.is_none(),
        "reset must NULL both new columns"
    );
}

#[tokio::test]
async fn mark_unsupported_source_writes_all_fields() {
    let (pool, video_id) = setup_with_video().await;
    mark_unsupported_source(&pool, video_id, 20).await.unwrap();
    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, \
                lyrics_processed_at, lyrics_alignment_model, lyrics_manual_priority \
         FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let has_lyrics: i64 = row.get("has_lyrics");
    let source: String = row.get("lyrics_source");
    let version: i64 = row.get("lyrics_pipeline_version");
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    let priority: i64 = row.get("lyrics_manual_priority");
    assert_eq!(has_lyrics, 0, "has_lyrics must be cleared");
    assert_eq!(
        source, "unsupported_source",
        "source must be sentinel literal"
    );
    assert_eq!(version, 20, "pipeline_version must be current");
    assert!(processed_at.is_some(), "processed_at must be set");
    assert!(model.is_none(), "alignment_model must be NULL");
    assert_eq!(priority, 0, "manual_priority must be cleared");
}

#[tokio::test]
async fn mark_unsupported_source_clears_manual_priority() {
    let (pool, video_id) = setup_with_video().await;
    // Seed with manual_priority = 1.
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(video_id)
        .execute(&pool)
        .await
        .unwrap();
    mark_unsupported_source(&pool, video_id, 20).await.unwrap();
    let priority: i64 =
        sqlx::query_scalar("SELECT lyrics_manual_priority FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        priority, 0,
        "manual_priority must be cleared by the sentinel"
    );
}

#[tokio::test]
async fn quarantine_video_lyrics_writes_processed_at_and_null_model() {
    // Focused assertion: the SQL extension in Task 2.5 must populate
    // lyrics_processed_at and leave lyrics_alignment_model NULL.
    let (pool, video_id) = setup_with_video().await;
    let tmp = tempfile::tempdir().unwrap();
    quarantine_video_lyrics(&pool, video_id, tmp.path(), "test reason", 20)
        .await
        .unwrap();
    let row =
        sqlx::query("SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(processed_at.is_some(), "quarantine must set processed_at");
    assert!(
        model.is_none(),
        "quarantine must leave alignment_model NULL"
    );
}
