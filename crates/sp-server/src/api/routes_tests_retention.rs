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
