//! `GET /api/v1/videos/:id/lyrics` for a song WITHOUT lyrics must be a normal
//! empty answer (204), never a 404 — the browser logs every 404 as a console
//! error, which failed the post-deploy zero-console gate on the 0.60.0 main
//! run (`post-deploy-flac.spec.ts` "idle playlists show the empty lyrics-view").
//! Included via `#[path = "routes_tests_lyrics_fetch.rs"] #[cfg(test)] mod
//! tests_lyrics_fetch;` from routes.rs; shares `test_state`/`app`.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn video_without_lyrics_answers_204_not_404() {
    let state = test_state().await;
    sqlx::query(
        "INSERT OR IGNORE INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'p', '', 'SP-t')",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics) \
         VALUES (77, 1, 'ytNOLYR', 1, 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/videos/77/lyrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unknown_video_id_stays_404() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/videos/999999/lyrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
