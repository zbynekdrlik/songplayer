//! #261 (lane 1's carried note 6229923904): a video imported into a playlist
//! while it is inactive is marked like the playlist's other songs (`removed_at`
//! = the playlist's `deactivated_at`, the mark its reactivation clears), so
//! #260's clock counts it; an active playlist's import and an existing row's
//! mark are left as they were.

use sqlx::SqlitePool;

use super::upsert_imported;
use crate::db::models_membership::follow_activation;

/// Playlist 1 (youtube) and 2 (`test` kind, both inactive at the start of no
/// test), with one earlier row so an import's id is never 1.
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for sql in [
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u1')",
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES (2, 't', 'u2', 'test')",
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES (5, 1, 'earlier0001')",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    pool
}

/// The playlist turned off (`active = false`) or on, as the PUT does it.
async fn set_active(pool: &SqlitePool, playlist_id: i64, active: bool, now_ms: i64) {
    let mut conn = pool.acquire().await.unwrap();
    follow_activation(&mut conn, playlist_id, active, now_ms)
        .await
        .unwrap();
    sqlx::query("UPDATE playlists SET is_active = ? WHERE id = ?")
        .bind(active)
        .bind(playlist_id)
        .execute(pool)
        .await
        .unwrap();
}

/// `(title, removed_at)` of the row with `id`.
async fn row(pool: &SqlitePool, id: i64) -> (Option<String>, Option<i64>) {
    sqlx::query_as("SELECT title, removed_at FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_video_imported_into_an_inactive_playlist_is_marked_like_its_songs() {
    let pool = pool().await;
    set_active(&pool, 1, false, 5_000).await;
    let id = upsert_imported(&pool, 1, "imported001", "Imported", Some(90_000))
        .await
        .unwrap();
    assert_ne!(id, 5, "a new row");
    assert_eq!(
        row(&pool, id).await,
        (Some("Imported".to_string()), Some(5_000)),
        "the deactivation's own mark"
    );
}

#[tokio::test]
async fn the_reactivation_clears_an_imported_songs_mark() {
    let pool = pool().await;
    set_active(&pool, 1, false, 5_000).await;
    let id = upsert_imported(&pool, 1, "imported001", "Imported", None)
        .await
        .unwrap();
    assert_eq!(row(&pool, id).await.1, Some(5_000), "marked while inactive");
    set_active(&pool, 1, true, 9_000).await;
    assert_eq!(row(&pool, id).await.1, None, "back with its playlist");
}

#[tokio::test]
async fn an_import_into_an_active_playlist_is_not_marked() {
    let pool = pool().await;
    let id = upsert_imported(&pool, 1, "imported001", "Imported", Some(1))
        .await
        .unwrap();
    let stored: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = 'imported001'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(id, stored, "the new row's id");
    assert_eq!(row(&pool, id).await, (Some("Imported".to_string()), None));
}

/// A row already there keeps whatever mark it has (a sync's, here) and takes
/// the title: the import marks only the row it adds.
#[tokio::test]
async fn a_re_import_keeps_the_rows_mark_and_takes_the_title() {
    let pool = pool().await;
    sqlx::query("UPDATE videos SET title = 'Old', removed_at = 777 WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    let id = upsert_imported(&pool, 1, "earlier0001", "New", None)
        .await
        .unwrap();
    assert_eq!(id, 5, "the existing row");
    assert_eq!(row(&pool, 5).await, (Some("New".to_string()), Some(777)));
}

/// The test item's playlist is never marked (lane 1's `deactivate` leaves its
/// `deactivated_at` empty), so neither is an import into it.
#[tokio::test]
async fn an_import_into_the_inactive_test_playlist_is_not_marked() {
    let pool = pool().await;
    set_active(&pool, 2, false, 5_000).await;
    let id = upsert_imported(&pool, 2, "testclip001", "Clip", None)
        .await
        .unwrap();
    assert_eq!(row(&pool, id).await.1, None);
}
