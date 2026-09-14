//! V23 migration tests (#152) — per-song SK translation gender + version
//! columns. Sibling file split from mod_tests.rs to honor the airuleset
//! 1000-line cap.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, column_names};
use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_v23_adds_lyrics_translation_gender_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_translation_gender".to_string()),
        "V23 must add lyrics_translation_gender column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v23_adds_lyrics_translation_version_column() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    assert!(
        cols.contains(&"lyrics_translation_version".to_string()),
        "V23 must add lyrics_translation_version column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v23_defaults_existing_rows_to_null_gender_and_zero_version() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 22).await;

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

    let gender: Option<String> =
        sqlx::query_scalar("SELECT lyrics_translation_gender FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let version: i64 =
        sqlx::query_scalar("SELECT lyrics_translation_version FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        gender.is_none(),
        "existing rows default to NULL lyrics_translation_gender (auto)"
    );
    assert_eq!(
        version, 0,
        "existing rows default to 0 lyrics_translation_version (never translated under the new prompt)"
    );
}

#[tokio::test]
async fn migration_v23_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().expect("at least one migration").0;
    assert!(latest >= 23, "V23 must be part of the migration list");
    assert_eq!(
        v, latest,
        "schema_version must advance to the newest migration ({latest}) after all migrations applied"
    );
}
