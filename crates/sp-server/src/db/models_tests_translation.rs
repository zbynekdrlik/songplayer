//! Tests for `models_translation` (#152) — per-song SK translation gender
//! override + translation-version bookkeeping. Registered as a submodule of
//! `models_translation` (not `models.rs`) so `models.rs` stays at/under the
//! 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;
use crate::db;
use sqlx::SqlitePool;

/// Seed one active playlist + one normalized, has_lyrics video row and return
/// the pool plus that video's id.
async fn setup_translated_video() -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (1, 'ytA', 't', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    (pool, id)
}

#[tokio::test]
async fn get_translation_gender_defaults_to_none() {
    let (pool, id) = setup_translated_video().await;
    let g = get_translation_gender(&pool, id).await.unwrap();
    assert_eq!(g, None, "a fresh row has NULL gender (auto)");
}

#[tokio::test]
async fn set_translation_gender_writes_value_and_resets_version() {
    let (pool, id) = setup_translated_video().await;
    // Pretend the song was already translated under version 1.
    stamp_translation_version(&pool, id, 1).await.unwrap();

    let affected = set_translation_gender(&pool, id, Some("f")).await.unwrap();
    assert!(affected, "set on an existing row reports a change");

    let g = get_translation_gender(&pool, id).await.unwrap();
    assert_eq!(g, Some("f".to_string()), "gender column stored");

    let version: i64 =
        sqlx::query_scalar("SELECT lyrics_translation_version FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        version, 0,
        "setting the gender resets the translation version so the song re-translates next pass"
    );
}

#[tokio::test]
async fn set_translation_gender_null_clears_to_auto() {
    let (pool, id) = setup_translated_video().await;
    set_translation_gender(&pool, id, Some("m")).await.unwrap();
    set_translation_gender(&pool, id, None).await.unwrap();
    let g = get_translation_gender(&pool, id).await.unwrap();
    assert_eq!(g, None, "passing None clears the override back to auto");
}

#[tokio::test]
async fn set_translation_gender_missing_row_reports_false() {
    let (pool, _id) = setup_translated_video().await;
    let affected = set_translation_gender(&pool, 9999, Some("m"))
        .await
        .unwrap();
    assert!(!affected, "no such video → no change");
}

#[tokio::test]
async fn stamp_translation_version_persists() {
    let (pool, id) = setup_translated_video().await;
    stamp_translation_version(&pool, id, 7).await.unwrap();
    let version: i64 =
        sqlx::query_scalar("SELECT lyrics_translation_version FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, 7);
}

#[tokio::test]
async fn fetch_next_stale_translation_picks_stale_active_row() {
    let (pool, id) = setup_translated_video().await;
    // has_lyrics=1, version 0 < 1, active playlist → eligible.
    let row = fetch_next_stale_translation(&pool, 1).await.unwrap();
    assert!(row.is_some(), "a stale-translation row must be selected");
    assert_eq!(row.unwrap().id, id);
}

#[tokio::test]
async fn fetch_next_stale_translation_skips_current_version() {
    let (pool, id) = setup_translated_video().await;
    stamp_translation_version(&pool, id, 1).await.unwrap();
    let row = fetch_next_stale_translation(&pool, 1).await.unwrap();
    assert!(
        row.is_none(),
        "a row already at the current translation version is not stale"
    );
}

#[tokio::test]
async fn fetch_next_stale_translation_skips_songs_without_lyrics() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (1, 'ytNoLyr', 't', 1, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = fetch_next_stale_translation(&pool, 1).await.unwrap();
    assert!(
        row.is_none(),
        "songs without lyrics have nothing to translate"
    );
}

#[tokio::test]
async fn fetch_next_stale_translation_skips_inactive_playlist() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 0)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (1, 'ytInactive', 't', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = fetch_next_stale_translation(&pool, 1).await.unwrap();
    assert!(row.is_none(), "inactive-playlist songs are not translated");
}

#[tokio::test]
async fn fetch_next_stale_translation_oldest_first() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let first: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (1, 'ytOld', 't', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (1, 'ytNew', 't', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = fetch_next_stale_translation(&pool, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.id, first,
        "oldest (lowest id) stale row is chosen first"
    );
}

#[tokio::test]
async fn fetch_next_stale_translation_skips_a_dub_subtitle_track() {
    // #184 H5 review: a dub's SK subtitles come from the Live-Translate session.
    // Re-translating that track with the lyrics translator would replace the
    // session's SK and drop the stored subtitle builder version.
    let (pool, id) = setup_translated_video().await;
    sqlx::query("UPDATE videos SET lyrics_source = 'gemini-live-translate' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        fetch_next_stale_translation(&pool, 1)
            .await
            .unwrap()
            .is_none(),
        "a Live-Translate dub subtitle track is never re-translated"
    );
    // Any other source (and NULL) stays eligible.
    sqlx::query("UPDATE videos SET lyrics_source = 'mtl' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        fetch_next_stale_translation(&pool, 1)
            .await
            .unwrap()
            .is_some()
    );
}
