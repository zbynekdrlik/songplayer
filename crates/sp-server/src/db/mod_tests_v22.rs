//! V22 migration tests (#144) — per-row lyrics retry backoff columns.
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
async fn migration_v22_adds_lyrics_attempts_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_attempts".to_string()),
        "V22 must add lyrics_attempts column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v22_adds_lyrics_next_attempt_at_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_next_attempt_at".to_string()),
        "V22 must add lyrics_next_attempt_at column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v22_defaults_existing_rows_to_zero_attempts_and_null_next_attempt() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 21).await;

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
    assert_eq!(attempts, 0, "existing rows default to 0 lyrics_attempts");
    assert!(
        next_attempt_at.is_none(),
        "existing rows default to NULL lyrics_next_attempt_at (eligible now)"
    );
}

#[tokio::test]
async fn migration_v22_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().expect("at least one migration").0;
    assert!(latest >= 22, "V22 must be part of the migration list");
    assert_eq!(
        v, latest,
        "schema_version must advance to the newest migration ({latest}) after all migrations applied"
    );
}
