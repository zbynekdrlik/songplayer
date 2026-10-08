//! V32 (#233 ruling 4): #210's VBAN keys deleted where the output list
//! exists; a box with no list keeps them for its outputs' one-time move.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

/// The keys of the settings V32 may touch, sorted.
async fn keys(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT key FROM settings WHERE key IN ('audio_outputs', 'gemini_model', \
         'vban_enabled', 'vban_stream_name', 'vban_targets') ORDER BY key",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A box at V31 with #210's keys and another setting, and the list when
/// `with_list`.
async fn box_at_v31(with_list: bool) -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 31).await;
    let mut rows = vec![
        ("vban_enabled", "true"),
        ("vban_stream_name", "sp-program"),
        ("vban_targets", "fohabl.lan:6980"),
        ("gemini_model", "m"),
    ];
    if with_list {
        rows.push(("audio_outputs", "[]"));
    }
    for (key, value) in rows {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

#[tokio::test]
async fn migration_v32_deletes_the_vban_keys_where_the_list_exists() {
    let pool = box_at_v31(true).await;
    apply_upto(&pool, 32).await;
    assert_eq!(keys(&pool).await, ["audio_outputs", "gemini_model"]);
}

#[tokio::test]
async fn migration_v32_keeps_the_vban_keys_with_no_list() {
    let pool = box_at_v31(false).await;
    apply_upto(&pool, 32).await;
    assert_eq!(
        keys(&pool).await,
        [
            "gemini_model",
            "vban_enabled",
            "vban_stream_name",
            "vban_targets"
        ]
    );
}

#[tokio::test]
async fn migration_v32_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, i64::from(MIGRATIONS.last().unwrap().0));
    assert_eq!(MIGRATIONS.last().unwrap().0, 32);
}
