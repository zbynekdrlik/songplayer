//! #132 — playlist CRUD must register / tear down the playback pipeline with
//! the engine, so a runtime-created playlist plays without a process restart.
//! Included via `#[path] #[cfg(test)] mod tests_runtime_pipeline;` from
//! routes.rs; shares `app`/`test_state_with_engine_rx` with `routes_tests.rs`
//! via `super::tests`. Kept in its own sibling file so `routes_tests.rs` stays
//! under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::tests::{app, test_state_with_engine_rx};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn create_playlist_sends_ensure_pipeline() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/playlists")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "name": "New",
                        "youtube_url": "https://youtube.com/playlist?list=PLnew",
                        "ndi_output_name": "SP-new"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = json["id"].as_i64().unwrap();
    // A newly created playlist is active by default (schema `is_active DEFAULT 1`),
    // which is why `create` must ensure its pipeline. Asserting the flag also pins
    // the `!= 0` decode in the response body.
    assert_eq!(json["is_active"], serde_json::json!(true));

    match engine_rx.try_recv() {
        Ok(crate::EngineCommand::EnsurePipeline { playlist_id }) => assert_eq!(playlist_id, id),
        other => panic!("create must send EnsurePipeline({id}), got {other:?}"),
    }
}

#[tokio::test]
async fn update_playlist_activate_sends_ensure_pipeline() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (5, 'p', 'u', 'SP-p', 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/playlists/5")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "is_active": true })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    match engine_rx.try_recv() {
        Ok(crate::EngineCommand::EnsurePipeline { playlist_id }) => assert_eq!(playlist_id, 5),
        other => panic!("activation must send EnsurePipeline(5), got {other:?}"),
    }
}

#[tokio::test]
async fn update_playlist_deactivate_sends_remove_pipeline() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (6, 'p', 'u', 'SP-p', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/playlists/6")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "is_active": false })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    match engine_rx.try_recv() {
        Ok(crate::EngineCommand::RemovePipeline { playlist_id }) => assert_eq!(playlist_id, 6),
        other => panic!("deactivation must send RemovePipeline(6), got {other:?}"),
    }
}

/// A non-activation update (e.g. rename / NDI-name set) still ensures the
/// pipeline — idempotent, and it covers the "NDI name set on an already-active
/// playlist" case that `create` alone would miss.
#[tokio::test]
async fn update_playlist_rename_sends_ensure_pipeline() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-p', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/playlists/7")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "ndi_output_name": "SP-renamed" }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    match engine_rx.try_recv() {
        Ok(crate::EngineCommand::EnsurePipeline { playlist_id }) => assert_eq!(playlist_id, 7),
        other => panic!("non-activation update must send EnsurePipeline(7), got {other:?}"),
    }
}

#[tokio::test]
async fn delete_playlist_sends_remove_pipeline() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (8, 'd', 'u', 'SP-d')",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/playlists/8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    match engine_rx.try_recv() {
        Ok(crate::EngineCommand::RemovePipeline { playlist_id }) => assert_eq!(playlist_id, 8),
        other => panic!("delete must send RemovePipeline(8), got {other:?}"),
    }
}
