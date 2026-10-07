//! V29 (#229): the node exchange's sha256 cache, `peer_hashes`.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

#[tokio::test]
async fn migration_v29_creates_the_peer_hash_cache_one_entry_per_path() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 28).await;
    apply_upto(&pool, 29).await;
    assert_eq!(
        column_names(&pool, "peer_hashes").await,
        vec!["path", "size", "mtime_ms", "sha256", "hashed_at_ms"]
    );
    let insert = "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) \
                  VALUES ('/c/a_audio.flac', 3, 4, 'ab', 5)";
    sqlx::query(insert).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(insert).execute(&pool).await.is_err(),
        "one entry per path"
    );
    assert_eq!(current_schema_version(&pool).await.unwrap(), 29);
}

#[tokio::test]
async fn migration_v29_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 29, "V29 must be part of the migration list");
    assert_eq!(current_schema_version(&pool).await.unwrap(), latest);
}
