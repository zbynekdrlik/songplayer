//! #261 (lane 1's carried note 6229923904): a video imported into a playlist
//! while it is inactive is marked like the playlist's other songs (`removed_at`
//! = the playlist's `deactivated_at`, the mark its reactivation clears), so
//! #260's clock counts it; an active playlist's import and an existing row's
//! mark are left as they were. The playlist is turned off and on through the
//! real route (`PUT /api/v1/playlists/{id}`), never a copy of its SQL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::SqlitePool;
use tower::ServiceExt;

use super::upsert_imported;
use crate::api::routes::tests::test_state;

/// Playlist 1 (youtube) and 2 (the test item's kind), with one earlier row
/// so an import's id is never 1.
async fn state() -> crate::AppState {
    let state = test_state().await;
    for sql in [
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u1')",
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES (2, 't', 'u2', 'test')",
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES (5, 1, 'earlier0001')",
    ] {
        sqlx::query(sql).execute(&state.pool).await.unwrap();
    }
    state
}

/// The playlist turned off (`active = false`) or on, through the real route.
async fn set_active(state: &crate::AppState, playlist_id: i64, active: bool) {
    let body = serde_json::json!({ "is_active": active }).to_string();
    let resp = crate::api::router(state.clone(), None)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/playlists/{playlist_id}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

async fn deactivated_at(pool: &SqlitePool, playlist_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT deactivated_at FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(pool)
        .await
        .unwrap()
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
    let state = state().await;
    set_active(&state, 1, false).await;
    let at = deactivated_at(&state.pool, 1).await.expect("stamped");
    let id = upsert_imported(&state.pool, 1, "imported001", "Imported", Some(90_000))
        .await
        .unwrap();
    assert_ne!(id, 5, "a new row");
    assert_eq!(
        row(&state.pool, id).await,
        (Some("Imported".to_string()), Some(at)),
        "the deactivation's own mark"
    );
}

#[tokio::test]
async fn the_reactivation_clears_an_imported_songs_mark() {
    let state = state().await;
    set_active(&state, 1, false).await;
    let at = deactivated_at(&state.pool, 1).await.expect("stamped");
    let id = upsert_imported(&state.pool, 1, "imported001", "Imported", None)
        .await
        .unwrap();
    assert_eq!(
        row(&state.pool, id).await.1,
        Some(at),
        "marked while inactive"
    );
    set_active(&state, 1, true).await;
    assert_eq!(row(&state.pool, id).await.1, None, "back with its playlist");
}

#[tokio::test]
async fn an_import_into_an_active_playlist_is_not_marked() {
    let state = state().await;
    let id = upsert_imported(&state.pool, 1, "imported001", "Imported", Some(1))
        .await
        .unwrap();
    let stored: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = 'imported001'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(id, stored, "the new row's id");
    assert_eq!(
        row(&state.pool, id).await,
        (Some("Imported".to_string()), None)
    );
}

/// A row already there keeps whatever mark it has (a sync's, here) and takes
/// the title: the import marks only the row it adds.
#[tokio::test]
async fn a_re_import_keeps_the_rows_mark_and_takes_the_title() {
    let state = state().await;
    sqlx::query("UPDATE videos SET title = 'Old', removed_at = 777 WHERE id = 5")
        .execute(&state.pool)
        .await
        .unwrap();
    let id = upsert_imported(&state.pool, 1, "earlier0001", "New", None)
        .await
        .unwrap();
    assert_eq!(id, 5, "the existing row");
    assert_eq!(
        row(&state.pool, 5).await,
        (Some("New".to_string()), Some(777))
    );
}

/// The test item's playlist is never marked (lane 1's `deactivate` leaves its
/// `deactivated_at` empty), so neither is an import into it.
#[tokio::test]
async fn an_import_into_the_inactive_test_playlist_is_not_marked() {
    let state = state().await;
    set_active(&state, 2, false).await;
    assert_eq!(deactivated_at(&state.pool, 2).await, None);
    let id = upsert_imported(&state.pool, 2, "testclip001", "Clip", None)
        .await
        .unwrap();
    assert_eq!(row(&state.pool, id).await.1, None);
}
