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

/// #300: the Dabing page's ▶ plays every row of the Dabing section on the
/// Dabing playlist, a dub-requested song of another playlist included, so
/// Loop takes such a song back after a restart — only while it is still in
/// the section (requested, downloaded, not removed). A youtube playlist never
/// takes another playlist's dub-requested song.
#[tokio::test]
async fn the_dabing_playlist_takes_a_dub_requested_song_of_another_playlist() {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES (4, 'Dabing', '', 'dabing')",
    )
    .execute(&pool)
    .await
    .unwrap();
    remember(&pool, 4, 21).await.unwrap();
    assert_eq!(
        remembered(&pool, 4).await.unwrap(),
        None,
        "not dub-requested: not in the Dabing section"
    );
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id IN (21, 13)")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(remembered(&pool, 4).await.unwrap(), Some(21));
    remember(&pool, 1, 21).await.unwrap();
    assert_eq!(
        remembered(&pool, 1).await.unwrap(),
        None,
        "a youtube playlist does not take another's dub-requested song"
    );
    remember(&pool, 4, 13).await.unwrap();
    assert_eq!(remembered(&pool, 4).await.unwrap(), None, "not downloaded");
    remember(&pool, 4, 21).await.unwrap();
    sqlx::query("UPDATE videos SET removed_at = 1 WHERE id = 21")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        remembered(&pool, 4).await.unwrap(),
        None,
        "left its YouTube playlist"
    );
}
