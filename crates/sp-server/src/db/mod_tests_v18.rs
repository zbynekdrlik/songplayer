//! V18 migration tests. Sibling file split from mod_tests.rs to honor
//! the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
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
    // Seed pre-V18 state: rewind schema_version to 17, hand-create the
    // spotify_track_id column (V17 already in fixture), then re-run
    // migrations. V18 must mark `spotify_resolved_at = datetime('now')`
    // for every row where `spotify_track_id IS NOT NULL`.
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();

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
    sqlx::query("UPDATE videos SET spotify_resolved_at = NULL WHERE id = ?")
        .bind(id_set)
        .execute(&pool)
        .await
        .unwrap();

    // Rewind schema_version + re-run V18 to exercise the backfill UPDATE.
    sqlx::query("DELETE FROM schema_version WHERE version = 18")
        .execute(&pool)
        .await
        .unwrap();
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
    run_migrations(&pool).await.unwrap();

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
    // Pre-emptively NULL the resolved_at column even if some prior step
    // touched it (defensive — the fresh INSERT defaults to NULL anyway).
    sqlx::query("UPDATE videos SET spotify_resolved_at = NULL WHERE id = ?")
        .bind(id_null)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("DELETE FROM schema_version WHERE version = 18")
        .execute(&pool)
        .await
        .unwrap();
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
