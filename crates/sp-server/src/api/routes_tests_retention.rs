//! #261 (no infinite archive): what the playlist handlers keep and drop.
//! Through the real router (`crate::api::router`) over a migrated in-memory
//! database. Included via `#[path] #[cfg(test)] mod tests_retention;` from
//! routes.rs, sharing `app` / `test_state` with `routes_tests.rs`.

use super::tests::{app, test_state};
use crate::playlist::selector::VideoSelector;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;
use tower::ServiceExt;

/// A request with no body.
async fn send(state: crate::AppState, method: &str, uri: &str) -> StatusCode {
    app(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// A custom playlist's set list as `(position, video_id)`, in order.
async fn set_list(pool: &SqlitePool, playlist_id: i64) -> Vec<(i64, i64)> {
    sqlx::query_as(
        "SELECT position, video_id FROM playlist_items WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn current_position(pool: &SqlitePool, playlist_id: i64) -> i64 {
    sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Playlists 1 (home) and 2 (to be deleted), both youtube, and 3 (a custom
/// set list): four downloaded songs, 12 the only one of playlist 2, and the
/// set list 11, 12, 13, 14 at positions 0..=3. Another set list (4) holds
/// only songs of playlist 1.
async fn two_set_lists(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES \
         (1, 'home', 'u1', 'youtube'), (2, 'gone', 'u2', 'youtube'), \
         (3, 'live', '', 'custom'), (4, 'other', '', 'custom')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path) VALUES \
         (11, 1, 'aaaaaaaaaaa', 1, '/c/a.mp4'), (12, 2, 'xxxxxxxxxxx', 1, '/c/x.mp4'), \
         (13, 1, 'bbbbbbbbbbb', 1, '/c/b.mp4'), (14, 1, 'ccccccccccc', 1, '/c/c.mp4')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlist_items (playlist_id, video_id, position) VALUES \
         (3, 11, 0), (3, 12, 1), (3, 13, 2), (3, 14, 3), (4, 13, 0), (4, 11, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// #261 bug: a playlist's DELETE cascades its songs out of every custom set
/// list and left a hole in the positions, so the selector asked the empty
/// position, wrapped to 0 and never reached the items after the hole. Now
/// every set list that lost an item is renumbered in the same transaction
/// and its current position follows the item it pointed at (13, playing,
/// moves from 2 to 1); a set list that lost nothing is left as it was.
#[tokio::test]
async fn deleting_a_playlist_renumbers_the_set_lists_that_held_its_songs() {
    let state = test_state().await;
    let pool = state.pool.clone();
    two_set_lists(&pool).await;
    sqlx::query("UPDATE playlists SET current_position = 2 WHERE id = 3")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        send(state, "DELETE", "/api/v1/playlists/2").await,
        StatusCode::NO_CONTENT
    );

    assert_eq!(set_list(&pool, 3).await, [(0, 11), (1, 13), (2, 14)]);
    assert_eq!(current_position(&pool, 3).await, 1, "13 is still current");
    assert_eq!(set_list(&pool, 4).await, [(0, 13), (1, 11)]);
    // The selector walks every item that is left, then wraps.
    let mut played = Vec::new();
    let mut current = Some(13);
    for _ in 0..4 {
        current = VideoSelector::select_next(&pool, 3, PlaybackMode::Continuous, current, &[])
            .await
            .unwrap();
        played.push(current.unwrap());
    }
    assert_eq!(played, [14, 11, 13, 14]);
}

/// An unknown playlist: 404, nothing written.
#[tokio::test]
async fn deleting_an_unknown_playlist_is_404() {
    let state = test_state().await;
    let pool = state.pool.clone();
    two_set_lists(&pool).await;
    assert_eq!(
        send(state, "DELETE", "/api/v1/playlists/99").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        set_list(&pool, 3).await,
        [(0, 11), (1, 12), (2, 13), (3, 14)]
    );
}

/// A PUT with a JSON body.
async fn put(state: crate::AppState, id: i64, body: serde_json::Value) -> StatusCode {
    app(state)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/playlists/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Every row's `removed_at`, by id.
async fn marks(pool: &SqlitePool) -> Vec<(i64, Option<i64>)> {
    sqlx::query_as("SELECT id, removed_at FROM videos ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn deactivated_at(pool: &SqlitePool, playlist_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT deactivated_at FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Playlist 1 (active) with `a` and `b` (`b` marked by the sync at 1 000),
/// playlist 2 with its own row of `a`, and the test item's playlist 3.
async fn three_playlists(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES \
         (1, 'p1', 'u1', 'youtube'), (2, 'p2', 'u2', 'youtube'), (3, 't', '', 'test')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, removed_at) VALUES \
         (11, 1, 'aaaaaaaaaaa', NULL), (12, 1, 'bbbbbbbbbbb', 1000), \
         (21, 2, 'aaaaaaaaaaa', NULL), (31, 3, 'ccccccccccc', NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// #261: a deactivation starts #260's clock on its playlist's rows (the
/// retention deletes them a year later), keeping a mark the sync set; a
/// second deactivation keeps the first one's time; a rename leaves the
/// marks alone; the reactivation clears exactly the deactivation's marks,
/// so the song that left its YouTube playlist stays out.
#[tokio::test]
async fn a_deactivation_marks_the_rows_and_a_reactivation_clears_only_its_marks() {
    let state = test_state().await;
    let pool = state.pool.clone();
    three_playlists(&pool).await;
    let before = chrono::Utc::now().timestamp_millis();

    let off = serde_json::json!({ "is_active": false });
    assert_eq!(
        put(state.clone(), 1, off.clone()).await,
        StatusCode::NO_CONTENT
    );
    let at = deactivated_at(&pool, 1).await.expect("stamped");
    assert!(at >= before, "{at} vs {before}");
    assert_eq!(
        marks(&pool).await,
        [(11, Some(at)), (12, Some(1000)), (21, None), (31, None)]
    );
    assert_eq!(deactivated_at(&pool, 2).await, None);

    sqlx::query("UPDATE playlists SET deactivated_at = 5000 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE videos SET removed_at = 5000 WHERE id = 11")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(put(state.clone(), 1, off).await, StatusCode::NO_CONTENT);
    assert_eq!(
        deactivated_at(&pool, 1).await,
        Some(5000),
        "the first time kept"
    );
    let rename = serde_json::json!({ "name": "renamed" });
    assert_eq!(put(state.clone(), 1, rename).await, StatusCode::NO_CONTENT);
    assert_eq!(
        marks(&pool).await,
        [(11, Some(5000)), (12, Some(1000)), (21, None), (31, None)]
    );

    let on = serde_json::json!({ "is_active": true });
    assert_eq!(put(state, 1, on).await, StatusCode::NO_CONTENT);
    assert_eq!(deactivated_at(&pool, 1).await, None);
    assert_eq!(
        marks(&pool).await,
        [(11, None), (12, Some(1000)), (21, None), (31, None)],
        "the sync's mark stays"
    );
}

/// The test item's playlist: its pinned clips are no songs to delete.
#[tokio::test]
async fn deactivating_the_test_items_playlist_marks_nothing() {
    let state = test_state().await;
    let pool = state.pool.clone();
    three_playlists(&pool).await;
    let off = serde_json::json!({ "is_active": false });
    assert_eq!(put(state, 3, off).await, StatusCode::NO_CONTENT);
    assert_eq!(deactivated_at(&pool, 3).await, None);
    assert_eq!(
        marks(&pool).await,
        [(11, None), (12, Some(1000)), (21, None), (31, None)]
    );
}

/// An `is_active` write waits for `cache::SONG_FILES` (the retention
/// deletes a row's files under it). The test never takes `MODE_ORDER`
/// while it holds the lock: a parallel test's activation holds that order
/// while it waits for the lock.
#[tokio::test]
async fn an_activation_write_waits_for_the_song_files_lock() {
    let state = test_state().await;
    let pool = state.pool.clone();
    three_playlists(&pool).await;
    let held = crate::downloader::cache::SONG_FILES.lock().await;
    let off = serde_json::json!({ "is_active": false });
    let write = tokio::spawn(put(state, 1, off));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!write.is_finished(), "the deactivation waits for the lock");
    assert_eq!(deactivated_at(&pool, 1).await, None, "nothing written yet");
    drop(held);
    let status = tokio::time::timeout(std::time::Duration::from_secs(60), write)
        .await
        .expect("it finishes once the lock is free")
        .unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(deactivated_at(&pool, 1).await.is_some());
}
