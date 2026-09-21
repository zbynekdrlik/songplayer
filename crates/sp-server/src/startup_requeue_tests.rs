//! Round G0 tests for [`requeue_unsupported_stems`]. Sibling of `startup.rs`,
//! wired via `#[path = "startup_requeue_tests.rs"]` so `startup.rs` stays under
//! the 1000-line airuleset cap.
//!
//! These prove the boot wrapper wires the raised 120-min cap
//! (`stems::worker::STEM_MAX_DURATION_MS`): a 36-min unsupported row is
//! re-opened (it would NOT be under the old 15-min cap) while a 121-min row
//! stays unsupported (it would be re-opened under an unbounded cap), and the
//! logged/returned count matches the rows flipped.

use super::*;
use crate::db;

async fn seed_pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// Insert a normalized, audio-bearing video with a duration, marked terminal
/// `unsupported` — the exact shape the old 15-min cap parked.
async fn seed_unsupported(pool: &SqlitePool, youtube_id: &str, duration_ms: i64) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos \
            (playlist_id, youtube_id, title, normalized, file_path, audio_file_path, \
             duration_ms, stem_status) \
         VALUES (1, ?, 't', 1, ?, ?, ?, 'unsupported') RETURNING id",
    )
    .bind(youtube_id)
    .bind(format!("/c/{youtube_id}_video.mp4"))
    .bind(format!("/c/{youtube_id}_audio.flac"))
    .bind(duration_ms)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

async fn stem_status_of(pool: &SqlitePool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn requeue_unsupported_stems_wires_the_120_min_cap() {
    let pool = seed_pool().await;

    // 36 min: within the raised 120-min cap → re-opened (would stay parked
    // under the old 15-min cap).
    let within = seed_unsupported(&pool, "in36", 36 * 60_000).await;
    // 121 min: past the 120-min ceiling → stays unsupported (would be re-opened
    // under an unbounded cap).
    let over = seed_unsupported(&pool, "ov121", 121 * 60_000).await;

    let requeued = requeue_unsupported_stems(&pool).await.unwrap();
    assert_eq!(requeued, 1, "only the within-cap row is re-opened");

    assert!(
        stem_status_of(&pool, within).await.is_none(),
        "a 36-min unsupported row must be re-opened (pending) by the boot wrapper"
    );
    assert_eq!(
        stem_status_of(&pool, over).await.as_deref(),
        Some("unsupported"),
        "a 121-min row stays unsupported — the wrapper wires the 120-min cap, not an unbounded one"
    );
}
