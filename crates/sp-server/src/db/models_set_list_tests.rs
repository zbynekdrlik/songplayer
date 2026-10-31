//! #261: a set list stays numbered `0..N-1`, its current position follows
//! its item.

use sqlx::SqlitePool;

use super::{delete_playlist, shifted_current};
use crate::db;

#[test]
fn the_current_position_follows_the_item_it_pointed_at() {
    // Nothing taken out.
    assert_eq!(shifted_current(2, &[0, 1, 2, 3]), 2);
    // An item before the current one taken out: the same item, one up.
    assert_eq!(shifted_current(2, &[0, 2, 3]), 1);
    // The current item taken out: the item before the next one, so the
    // next pick is the item that followed it.
    assert_eq!(shifted_current(1, &[0, 2, 3]), 0);
    assert_eq!(shifted_current(3, &[0, 1, 4]), 1);
    // The current item taken out with none kept before it: the last item,
    // so the next pick wraps to the first.
    assert_eq!(shifted_current(0, &[1, 2]), 1);
    assert_eq!(shifted_current(0, &[1, 2, 5]), 2);
    // Every item after it taken out: the last one, the next pick wraps.
    assert_eq!(shifted_current(5, &[0, 1]), 1);
    // An empty set list.
    assert_eq!(shifted_current(3, &[]), 0);
}

async fn pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    pool
}

/// Playlists 1 and 2 (youtube) with songs 11, 13 (playlist 1) and 12, 15
/// (playlist 2); set list 3 = 11, 12, 13, 15 at 0..=3, current 1 (12, a
/// song of the playlist to be deleted).
async fn rig(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind, current_position) VALUES \
         (1, 'home', 'u1', 'youtube', 0), (2, 'gone', 'u2', 'youtube', 0), \
         (3, 'live', '', 'custom', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES \
         (11, 1, 'aaaaaaaaaaa'), (12, 2, 'xxxxxxxxxxx'), (13, 1, 'bbbbbbbbbbb'), \
         (15, 2, 'yyyyyyyyyyy')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlist_items (playlist_id, video_id, position) VALUES \
         (3, 11, 0), (3, 12, 1), (3, 13, 2), (3, 15, 3)",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn set_list(pool: &SqlitePool, playlist_id: i64) -> (Vec<(i64, i64)>, i64) {
    let items: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT position, video_id FROM playlist_items WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .unwrap();
    let current: i64 = sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(pool)
        .await
        .unwrap();
    (items, current)
}

/// Two items of the deleted playlist, one of them the current one: the
/// rest is numbered again and the next pick (current + 1) is 13, the item
/// that followed the one taken out.
#[tokio::test]
async fn a_set_list_that_loses_two_items_is_numbered_again() {
    let pool = pool().await;
    rig(&pool).await;
    assert!(delete_playlist(&pool, 2).await.unwrap());
    assert_eq!(set_list(&pool, 3).await, (vec![(0, 11), (1, 13)], 0));
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE id = 2")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// An unknown playlist: `false`, nothing written.
#[tokio::test]
async fn deleting_an_unknown_playlist_changes_nothing() {
    let pool = pool().await;
    rig(&pool).await;
    assert!(!delete_playlist(&pool, 99).await.unwrap());
    assert_eq!(
        set_list(&pool, 3).await,
        (vec![(0, 11), (1, 12), (2, 13), (3, 15)], 1)
    );
}

/// The custom playlist itself: its own items go with it, every other set
/// list is left as it was.
#[tokio::test]
async fn deleting_a_custom_playlist_takes_only_its_own_items() {
    let pool = pool().await;
    rig(&pool).await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind, current_position) \
         VALUES (4, 'other', '', 'custom', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO playlist_items (playlist_id, video_id, position) VALUES (4, 13, 0)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(delete_playlist(&pool, 3).await.unwrap());
    let items: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM playlist_items WHERE playlist_id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(items, 0);
    assert_eq!(set_list(&pool, 4).await, (vec![(0, 13)], 0));
}

/// The set-list removal shares the numbering: an item taken out before the
/// current one keeps the current item current.
#[tokio::test]
async fn removing_an_item_before_the_current_one_keeps_the_current_item() {
    let pool = pool().await;
    rig(&pool).await;
    sqlx::query("UPDATE playlists SET current_position = 2 WHERE id = 3")
        .execute(&pool)
        .await
        .unwrap();
    db::models::remove_playlist_item(&pool, 3, 11)
        .await
        .unwrap();
    assert_eq!(
        set_list(&pool, 3).await,
        (vec![(0, 12), (1, 13), (2, 15)], 1),
        "13 is still current, 15 the next pick"
    );
}
