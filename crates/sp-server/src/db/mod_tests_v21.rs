//! V21 migration tests (#142) — reference-lyrics marker + feedback columns.
//! Sibling file split from mod_tests.rs to honor the airuleset 1000-line cap.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, column_names};
use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_v21_adds_lyrics_reference_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_reference".to_string()),
        "V21 must add lyrics_reference column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v21_adds_lyrics_reference_rejected_at_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_reference_rejected_at".to_string()),
        "V21 must add lyrics_reference_rejected_at column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v21_adds_lyrics_reference_note_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_reference_note".to_string()),
        "V21 must add lyrics_reference_note column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v21_defaults_existing_rows_to_zero_reference_and_null_feedback() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 20).await;

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (1, 'aaa', 't') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    run_migrations(&pool).await.unwrap();

    let reference: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let rejected_at: Option<String> =
        sqlx::query_scalar("SELECT lyrics_reference_rejected_at FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let note: Option<String> =
        sqlx::query_scalar("SELECT lyrics_reference_note FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reference, 0, "existing rows default to 0 (not reference)");
    assert!(
        rejected_at.is_none(),
        "existing rows default to NULL rejected_at"
    );
    assert!(note.is_none(), "existing rows default to NULL note");
}

#[tokio::test]
async fn migration_v21_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().expect("at least one migration").0;
    assert!(latest >= 21, "V21 must be part of the migration list");
    assert_eq!(
        v, latest,
        "schema_version must advance to the newest migration ({latest}) after all migrations applied"
    );
}
