//! V26 migration tests (#180 — dubbing D1) — dub bookkeeping columns on
//! `videos` + `stem_manual_priority`. Sibling file split from `mod_tests.rs`
//! to honor the airuleset 1000-line cap; mirrors the V24 stems test shape.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, column_names};
use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn migration_v26_adds_all_dub_columns() {
    let pool = setup().await;
    let cols = column_names(&pool, "videos").await;
    for c in [
        "dub_requested",
        "dub_status",
        "dub_file_path",
        "dub_engine",
        "dub_voice_ref_path",
        "dub_mix_ratio",
        "dub_error",
        "dub_attempts",
        "dub_next_attempt_at",
        "dub_requested_at",
        "stem_manual_priority",
    ] {
        assert!(
            cols.contains(&c.to_string()),
            "V26 must add {c} column; got: {cols:?}"
        );
    }
}

#[tokio::test]
async fn migration_v26_defaults_existing_rows() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 25).await;

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

    let dub_requested: i64 =
        sqlx::query_scalar("SELECT dub_requested FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let dub_status: String = sqlx::query_scalar("SELECT dub_status FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let dub_mix_ratio: f64 =
        sqlx::query_scalar("SELECT dub_mix_ratio FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let stem_manual_priority: i64 =
        sqlx::query_scalar("SELECT stem_manual_priority FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(dub_requested, 0, "existing rows default to dub_requested=0");
    assert_eq!(dub_status, "none", "existing rows default to dub_status='none'");
    assert_eq!(
        dub_mix_ratio, 1.0,
        "existing rows default to dub_mix_ratio=1.0"
    );
    assert_eq!(
        stem_manual_priority, 0,
        "existing rows default to stem_manual_priority=0"
    );
}

#[tokio::test]
async fn migration_v26_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    let latest = MIGRATIONS.last().expect("at least one migration").0;
    assert!(latest >= 26, "V26 must be part of the migration list");
    assert_eq!(
        v, latest,
        "schema_version must advance to the newest migration ({latest}) after all migrations applied"
    );
}
