//! V24 migration tests (#14) — karaoke stem separation columns. Sibling file
//! split from mod_tests.rs to honor the airuleset 1000-line cap.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, column_names};
use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_v24_adds_all_stem_columns() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    for c in [
        "vocals_file_path",
        "instrumental_file_path",
        "stem_status",
        "stem_attempts",
        "stem_next_attempt_at",
    ] {
        assert!(
            cols.contains(&c.to_string()),
            "V24 must add {c} column; got: {cols:?}"
        );
    }
}

#[tokio::test]
async fn migration_v24_defaults_existing_rows_to_pending() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 23).await;

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

    let status: Option<String> = sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let attempts: i64 = sqlx::query_scalar("SELECT stem_attempts FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let vpath: Option<String> =
        sqlx::query_scalar("SELECT vocals_file_path FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        status.is_none(),
        "existing rows default to NULL stem_status (pending)"
    );
    assert_eq!(attempts, 0, "existing rows default to 0 stem_attempts");
    assert!(
        vpath.is_none(),
        "existing rows have no vocals_file_path yet"
    );
}

#[tokio::test]
async fn migration_v24_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().expect("at least one migration").0;
    assert!(latest >= 24, "V24 must be part of the migration list");
    assert_eq!(
        v, latest,
        "schema_version must advance to the newest migration ({latest}) after all migrations applied"
    );
}
