//! #250: the register check's V36 columns.

use sqlx::SqlitePool;

use super::{fetch_next_stale_register, stamp_register};
use crate::db;

/// An active playlist 1 (and an inactive 2) and no video yet.
async fn pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active) \
         VALUES (1, 'p', 'u', 1), (2, 'q', 'v', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A video row: `(playlist, youtube_id, normalized, has_lyrics)`.
async fn video(pool: &SqlitePool, playlist: i64, yt: &str, normalized: i64, lyrics: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) \
         VALUES (?, ?, 't', ?, ?) RETURNING id",
    )
    .bind(playlist)
    .bind(yt)
    .bind(normalized)
    .bind(lyrics)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn columns(pool: &SqlitePool, id: i64) -> (i64, Option<String>) {
    sqlx::query_as(
        "SELECT lyrics_register_version, lyrics_register_verdict FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_row_starts_unchecked() {
    let pool = pool().await;
    let id = video(&pool, 1, "ytA", 1, 1).await;
    assert_eq!(columns(&pool, id).await, (0, None));
}

#[tokio::test]
async fn the_stamp_goes_on_every_row_of_the_video_only() {
    let pool = pool().await;
    let a = video(&pool, 1, "ytA", 1, 1).await;
    let a_again = video(&pool, 2, "ytA", 1, 1).await;
    let b = video(&pool, 1, "ytB", 1, 1).await;
    stamp_register(&pool, a, 3, Some("fixed")).await.unwrap();
    assert_eq!(columns(&pool, a).await, (3, Some("fixed".into())));
    assert_eq!(columns(&pool, a_again).await, (3, Some("fixed".into())));
    assert_eq!(columns(&pool, b).await, (0, None));
    stamp_register(&pool, a_again, 4, None).await.unwrap();
    assert_eq!(columns(&pool, a).await, (4, None));
}

#[tokio::test]
async fn the_stale_pass_takes_the_lowest_unchecked_served_row() {
    let pool = pool().await;
    let first = video(&pool, 1, "ytA", 1, 1).await;
    let second = video(&pool, 1, "ytB", 1, 1).await;
    assert_eq!(
        fetch_next_stale_register(&pool, 1).await.unwrap(),
        Some((first, "ytA".to_string()))
    );
    stamp_register(&pool, first, 1, Some("ok")).await.unwrap();
    assert_eq!(
        fetch_next_stale_register(&pool, 1).await.unwrap(),
        Some((second, "ytB".to_string()))
    );
    stamp_register(&pool, second, 1, Some("ok")).await.unwrap();
    assert_eq!(fetch_next_stale_register(&pool, 1).await.unwrap(), None);
    // A newer version makes both stale again, the lowest first.
    assert_eq!(
        fetch_next_stale_register(&pool, 2).await.unwrap(),
        Some((first, "ytA".to_string()))
    );
}

#[tokio::test]
async fn the_stale_pass_leaves_rows_with_nothing_to_check() {
    let pool = pool().await;
    video(&pool, 1, "ytNoLyrics", 1, 0).await;
    video(&pool, 1, "ytNotDownloaded", 0, 1).await;
    video(&pool, 2, "ytInactive", 1, 1).await;
    let dub = video(&pool, 1, "ytDub", 1, 1).await;
    sqlx::query("UPDATE videos SET lyrics_source = 'gemini-live-translate' WHERE id = ?")
        .bind(dub)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(fetch_next_stale_register(&pool, 1).await.unwrap(), None);
    // Any other source is checked.
    sqlx::query("UPDATE videos SET lyrics_source = 'gemini-3-5-transcribe' WHERE id = ?")
        .bind(dub)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        fetch_next_stale_register(&pool, 1).await.unwrap(),
        Some((dub, "ytDub".to_string()))
    );
}
