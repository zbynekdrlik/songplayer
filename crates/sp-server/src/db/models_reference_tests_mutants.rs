//! Mutation-killing unit tests for `models_reference.rs` (the ★ flag's
//! writer and the "Nesedí" feedback; #241 deleted the per-song read).
//!
//! Wired into `models_reference.rs` as a sibling `#[path]` test module. Uses
//! an in-memory sqlite pool + real migrations, seeds two videos rows (one
//! with `lyrics_reference = 1`, one with `= 0`), and pins the true/false pair
//! — killing the `Ok(true)` / `Ok(false)` body mutants and the `!= -> ==`
//! comparison mutant at once.

use super::*;
use crate::db;

/// Seed a playlist (FK parent) + two videos, one starred and one not.
async fn setup_two_videos() -> (SqlitePool, i64, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) VALUES (1, 'p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id_ref: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
         VALUES (1, 'ytRef', 't', 0) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let id_plain: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
         VALUES (1, 'ytPlain', 't', 0) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    (pool, id_ref, id_plain)
}

/// The flag as the database holds it (`videos.lyrics_reference`).
async fn stored_reference(pool: &SqlitePool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `set_video_lyrics_reference` sets the flag on one row and clears it on
/// another, and answers how many rows it touched (0 for no such video).
#[tokio::test]
async fn set_video_lyrics_reference_sets_and_clears_the_flag() {
    let (pool, id_ref, id_plain) = setup_two_videos().await;
    assert_eq!(
        set_video_lyrics_reference(&pool, id_ref, true)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        set_video_lyrics_reference(&pool, id_plain, false)
            .await
            .unwrap(),
        1
    );
    assert_eq!(stored_reference(&pool, id_ref).await, 1);
    assert_eq!(stored_reference(&pool, id_plain).await, 0);
    assert_eq!(
        set_video_lyrics_reference(&pool, 999_999, true)
            .await
            .unwrap(),
        0
    );
}

/// #144 review round 2: the "Nesedí" feedback queues the song like the
/// reprocess routes do — a fresh attempt budget and no backoff left
/// (`lyrics_attempts = 0`, `lyrics_next_attempt_at = NULL`), so a song the
/// wall serves gets its `SERVED_RERUN_MAX_ATTEMPTS` failed attempts before
/// it leaves the manual queue, and is picked at once.
#[tokio::test]
async fn reference_feedback_queues_the_song_with_a_fresh_attempt_budget() {
    let (pool, id_ref, _id_plain) = setup_two_videos().await;
    sqlx::query(
        "UPDATE videos SET lyrics_attempts = 2, \
         lyrics_next_attempt_at = '2999-01-01T00:00:00.000Z' WHERE id = ?",
    )
    .bind(id_ref)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        record_reference_feedback(&pool, id_ref, "off")
            .await
            .unwrap(),
        1
    );

    let row: (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT lyrics_manual_priority, lyrics_attempts, lyrics_next_attempt_at \
         FROM videos WHERE id = ?",
    )
    .bind(id_ref)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (1, 0, None));
}
