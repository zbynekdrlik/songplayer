//! Mutation-killing unit tests for `models_reference.rs::get_video_lyrics_reference`.
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

/// Kills all three `18:5` / `22:20` mutants:
///   - `-> Ok(true)`: the `id_plain` row would read `true` instead of `false`.
///   - `-> Ok(false)`: the `id_ref` row would read `false` instead of `true`.
///   - `22:20 != -> ==`: `1 == 0` → false for the starred row, `0 == 0` → true
///     for the plain row — both asserts flip.
#[tokio::test]
async fn get_video_lyrics_reference_reads_flag_true_and_false() {
    let (pool, id_ref, id_plain) = setup_two_videos().await;

    set_video_lyrics_reference(&pool, id_ref, true)
        .await
        .unwrap();
    set_video_lyrics_reference(&pool, id_plain, false)
        .await
        .unwrap();

    assert!(
        get_video_lyrics_reference(&pool, id_ref).await.unwrap(),
        "a row with lyrics_reference=1 must read true"
    );
    assert!(
        !get_video_lyrics_reference(&pool, id_plain).await.unwrap(),
        "a row with lyrics_reference=0 must read false"
    );
}

/// A missing row reads `false` (`.unwrap_or(false)`). Redundantly reinforces
/// the `Ok(true)` body mutant (which would return `true` for no such row).
#[tokio::test]
async fn get_video_lyrics_reference_missing_row_is_false() {
    let (pool, _id_ref, _id_plain) = setup_two_videos().await;
    assert!(
        !get_video_lyrics_reference(&pool, 999_999).await.unwrap(),
        "a non-existent video id must read false"
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
