//! V19 migration tests. Sibling file split from mod_tests.rs to honor
//! the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

/// Apply V1..V18 manually so a test can seed pre-V19 data and then trigger
/// V19 in isolation. Mirrors `apply_through_v17` in `mod_tests_v18.rs`.
async fn apply_through_v18(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await
    .unwrap();

    // MIGRATIONS[..18] is V1..V18 inclusive (18 entries), leaving V19
    // (the last one) for the caller to apply via run_migrations.
    for &(version, sql) in &MIGRATIONS[..18] {
        let mut tx = pool.begin().await.unwrap();
        for stmt in sql.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&mut *tx).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
            .bind(version)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn migration_v19_adds_lyrics_processed_at_column() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        cols.contains(&"lyrics_processed_at".to_string()),
        "V19 must add lyrics_processed_at column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v19_adds_lyrics_alignment_model_column() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        cols.contains(&"lyrics_alignment_model".to_string()),
        "V19 must add lyrics_alignment_model column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v19_leaves_existing_rows_with_null_for_new_columns() {
    let pool = create_memory_pool().await.unwrap();
    apply_through_v18(&pool).await;

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

    let processed_at: Option<String> =
        sqlx::query_scalar("SELECT lyrics_processed_at FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let model: Option<String> =
        sqlx::query_scalar("SELECT lyrics_alignment_model FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        processed_at.is_none() && model.is_none(),
        "V19 must NOT backfill existing rows — they stay NULL"
    );
}

#[tokio::test]
async fn migration_v19_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    assert_eq!(v, 19, "schema_version must advance to 19 after V19 applied");
}
