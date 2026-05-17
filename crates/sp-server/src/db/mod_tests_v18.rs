//! V18 migration tests. Sibling file split from mod_tests.rs to honor
//! the airuleset 1000-line cap.

use super::test_helpers::apply_first_n;
use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_v18_adds_spotify_resolved_at_column() {
    let pool = setup().await;
    let cols = super::test_helpers::column_names(&pool, "videos").await;
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
    apply_first_n(&pool, 17).await;

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
    apply_first_n(&pool, 17).await;

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
