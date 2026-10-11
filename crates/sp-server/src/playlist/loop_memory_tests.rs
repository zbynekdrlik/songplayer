//! #229 follow-up: the remembered Loop song.

use sqlx::SqlitePool;

use super::{remember, remember_logged, remembered};
use crate::db;

/// A youtube playlist 1 with downloaded videos 11 and 12 and a video 13 not
/// downloaded; a youtube playlist 2 with video 21; a custom playlist 3.
async fn pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES (3, 'c', '', 'custom')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) VALUES \
         (11, 1, 'v11', 1), (12, 1, 'v12', 1), (13, 1, 'v13', 0), (21, 2, 'v21', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

#[tokio::test]
async fn nothing_is_remembered_at_first() {
    let pool = pool().await;
    assert_eq!(remembered(&pool, 1).await.unwrap(), None);
    assert_eq!(
        remembered(&pool, 99).await.unwrap(),
        None,
        "no such playlist"
    );
}

#[tokio::test]
async fn the_last_song_remembered_is_the_one() {
    let pool = pool().await;
    remember(&pool, 1, 11).await.unwrap();
    assert_eq!(remembered(&pool, 1).await.unwrap(), Some(11));
    remember_logged(&pool, 1, 12).await;
    assert_eq!(remembered(&pool, 1).await.unwrap(), Some(12));
    assert_eq!(remembered(&pool, 2).await.unwrap(), None, "per playlist");
}

#[tokio::test]
async fn a_song_that_cannot_play_there_is_not_taken() {
    let pool = pool().await;
    remember(&pool, 1, 13).await.unwrap();
    assert_eq!(remembered(&pool, 1).await.unwrap(), None, "not downloaded");
    remember(&pool, 1, 21).await.unwrap();
    assert_eq!(
        remembered(&pool, 1).await.unwrap(),
        None,
        "another playlist's"
    );
    remember(&pool, 1, 777).await.unwrap();
    assert_eq!(remembered(&pool, 1).await.unwrap(), None, "deleted");
}

#[tokio::test]
async fn a_custom_playlist_takes_a_song_of_its_items() {
    let pool = pool().await;
    remember(&pool, 3, 21).await.unwrap();
    assert_eq!(
        remembered(&pool, 3).await.unwrap(),
        None,
        "not one of its items"
    );
    sqlx::query("INSERT INTO playlist_items (playlist_id, video_id, position) VALUES (3, 21, 0)")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(remembered(&pool, 3).await.unwrap(), Some(21));
}
