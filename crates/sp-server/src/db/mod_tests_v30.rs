//! V30 (#229): ask first — the waits and the provenance of fetched artifacts.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

#[tokio::test]
async fn migration_v30_creates_the_wait_and_fetch_tables() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 29).await;
    apply_upto(&pool, 30).await;
    assert_eq!(
        column_names(&pool, "peer_waits").await,
        vec!["youtube_id", "job", "since_ms"]
    );
    assert_eq!(
        column_names(&pool, "peer_fetches").await,
        vec![
            "youtube_id",
            "kind",
            "node",
            "version",
            "sha256",
            "fetched_at_ms"
        ]
    );
    let wait =
        "INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES ('aaaaaaaaaaa', 'stems', 1)";
    sqlx::query(wait).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(wait).execute(&pool).await.is_err(),
        "one wait per video + job"
    );
    sqlx::query(
        "INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES ('aaaaaaaaaaa', 'lyrics', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let fetch = "INSERT INTO peer_fetches (youtube_id, kind, node, version, sha256, fetched_at_ms) \
                 VALUES ('aaaaaaaaaaa', 'audio', 'snv', 1, 'ab', 2)";
    sqlx::query(fetch).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(fetch).execute(&pool).await.is_err(),
        "one record per video + kind"
    );
    assert_eq!(current_schema_version(&pool).await.unwrap(), 30);
}

#[tokio::test]
async fn migration_v30_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 30, "V30 must be part of the migration list");
    assert_eq!(current_schema_version(&pool).await.unwrap(), latest);
}
