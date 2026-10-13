//! V40 (#260): when a row's song left its YouTube playlist, NULL for every
//! row before (every song of an old database is in its playlist).

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v40_adds_the_removed_mark_null_for_old_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 39).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (40001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (40011, 40001, 'v40')")
        .execute(&pool)
        .await
        .unwrap();
    apply_upto(&pool, 40).await;
    let removed: Option<i64> = sqlx::query_scalar("SELECT removed_at FROM videos WHERE id = 40011")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(removed, None);
    sqlx::query("UPDATE videos SET removed_at = 1760000000000 WHERE id = 40011")
        .execute(&pool)
        .await
        .unwrap();
    let removed: Option<i64> = sqlx::query_scalar("SELECT removed_at FROM videos WHERE id = 40011")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(removed, Some(1_760_000_000_000));
}

/// The retention reads the marked rows by this index.
#[tokio::test]
async fn v40_indexes_the_marked_rows() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let sql: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_videos_removed_at'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    let sql = sql.expect("the index exists");
    assert!(sql.contains("WHERE removed_at IS NOT NULL"), "{sql}");
}

#[tokio::test]
async fn migration_v40_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 40, "{latest}");
}
