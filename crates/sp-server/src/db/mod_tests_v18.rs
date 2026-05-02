//! V18 migration tests. Sibling file split from mod_tests.rs to honor
//! the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

/// Apply V1..V17 (the 17 migrations BEFORE V18) manually so a test can seed
/// pre-V18 data and then trigger V18 in isolation. Mirrors the pattern in
/// `migration_v4_resets_all_normalized_rows` (mod_tests.rs:46).
async fn apply_through_v17(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await
    .unwrap();

    // MIGRATIONS is 0-indexed; [..17] is V1..V17 inclusive (17 entries),
    // leaving V18 (the last one) for the caller to apply via run_migrations.
    for &(version, sql) in &MIGRATIONS[..17] {
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
async fn migration_v18_adds_spotify_resolved_at_column() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        cols.contains(&"spotify_resolved_at".to_string()),
        "V18 must add spotify_resolved_at column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v18_backfills_resolved_at_for_existing_track_ids() {
    // Apply V1..V17 manually so we can seed a row with spotify_track_id set
    // BEFORE V18 fires. Then run_migrations applies V18 (the only remaining
    // migration) and the test asserts the backfill UPDATE marked the row.
    let pool = create_memory_pool().await.unwrap();
    apply_through_v17(&pool).await;

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let id_set: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, spotify_track_id) \
         VALUES (1, 'aaa', 't', '3n3Ppam7vgaVa1iaRUc9Lp') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Apply V18 only — V1..V17 already in schema_version.
    run_migrations(&pool).await.unwrap();

    let resolved: Option<String> =
        sqlx::query_scalar("SELECT spotify_resolved_at FROM videos WHERE id = ?")
            .bind(id_set)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        resolved.is_some(),
        "V18 backfill must set spotify_resolved_at for rows with non-NULL spotify_track_id"
    );
}

#[tokio::test]
async fn migration_v18_leaves_null_track_id_rows_unbackfilled() {
    let pool = create_memory_pool().await.unwrap();
    apply_through_v17(&pool).await;

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    // Row with NULL spotify_track_id (the common case post-V18 for fresh rows).
    let id_null: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (1, 'bbb', 't2') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Apply V18 only.
    run_migrations(&pool).await.unwrap();

    let resolved: Option<String> =
        sqlx::query_scalar("SELECT spotify_resolved_at FROM videos WHERE id = ?")
            .bind(id_null)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        resolved.is_none(),
        "V18 backfill WHERE clause must NOT touch rows where spotify_track_id IS NULL"
    );
}
