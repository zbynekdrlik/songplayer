//! Unit + axum integration tests for the REST handlers in `routes.rs`.
//!
//! Included as a sibling file via `#[cfg(test)] #[path = "routes_tests.rs"]
//! mod tests;` from routes.rs so the handlers file stays under 1000 lines
//! (airuleset file-size check).

#![allow(unused_imports)]

use super::*;
use crate::AppState;
use crate::db;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};
use tower::ServiceExt;

async fn test_state() -> AppState {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let (event_tx, _) = broadcast::channel(16);
    let (engine_tx, _) = mpsc::channel(16);
    let (sync_tx, _) = mpsc::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (obs_rebuild_tx, _) = broadcast::channel(4);
    AppState {
        pool,
        event_tx,
        engine_tx,
        obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
        tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
        tool_paths: Arc::new(RwLock::new(None)),
        sync_tx,
        resolume_tx,
        obs_rebuild_tx,
    }
}

fn app(state: AppState) -> axum::Router {
    crate::api::router(state, None)
}

#[tokio::test]
async fn status_returns_200() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["version"].is_string());
    assert_eq!(json["obs_connected"], false);
    assert_eq!(json["playlist_count"], 0);
}

#[tokio::test]
async fn create_and_list_playlists() {
    let state = test_state().await;
    let app = app(state);

    // Create
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/playlists")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "name": "Test",
                        "youtube_url": "https://youtube.com/playlist?list=PLtest"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);

    // List
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/playlists")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.len(), 1);
    assert_eq!(json[0]["name"], "Test");
}

#[tokio::test]
async fn get_playlist_not_found() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/playlists/999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_playlist_not_found() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/playlists/999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_get_empty() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.is_object());
}

#[tokio::test]
async fn settings_patch_and_get() {
    let state = test_state().await;
    let app = app(state);

    // Patch
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "obs_websocket_url": "ws://10.0.0.1:4455",
                        "cache_dir": "/tmp/cache"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Get
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["obs_websocket_url"], "ws://10.0.0.1:4455");
    assert_eq!(json["cache_dir"], "/tmp/cache");
}

#[tokio::test]
async fn resolume_hosts_crud() {
    let state = test_state().await;
    let app = app(state);

    // Add host
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/resolume/hosts")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "label": "Main Resolume",
                        "host": "192.168.1.10",
                        "port": 8090
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
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let host_id = created["id"].as_i64().unwrap();

    // List hosts
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/resolume/hosts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let hosts: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(hosts.len(), 1);

    // Delete host
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/resolume/hosts/{host_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn playback_play_returns_no_content() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/playback/1/play")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn playback_previous_returns_no_content() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/playback/1/previous")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// Regression for issue #8: the dashboard used to POST to
/// `/api/v1/control` which does not exist on the server, returning 405
/// from the static-file fallback router. The dashboard now uses the
/// path-based endpoints; this test asserts that the bogus legacy path
/// is still NOT handled, which prevents accidental re-introduction.
#[tokio::test]
async fn legacy_control_path_is_not_handled() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/control")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"type":"Play","data":{"playlist_id":1}}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Without a static-file fallback in this test (None dist_dir), the
    // router's default behaviour for an unknown path is 404. Either 404
    // or 405 is acceptable — what matters is that the path is NOT a
    // successful 2xx, i.e. the dashboard MUST NOT rely on it.
    assert!(
        !resp.status().is_success(),
        "/api/v1/control must not be a valid playback endpoint"
    );
}

#[tokio::test]
async fn status_json_shape() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: StatusResponse = serde_json::from_slice(&body).unwrap();
    assert!(!json.version.is_empty());
    assert!(!json.obs_connected);
    assert!(!json.tools.ytdlp_available);
    assert!(!json.tools.ffmpeg_available);
    // Fresh state → no playlists active on program yet.
    assert!(json.active_playlist_ids.is_empty());
}

/// Playlist CRUD must signal the OBS client to rebuild its NDI source
/// map — otherwise newly-added playlists never get scene-matched.
#[tokio::test]
async fn create_playlist_sends_obs_rebuild_signal() {
    let state = test_state().await;
    let mut rebuild_rx = state.obs_rebuild_tx.subscribe();
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

    // Rebuild signal must arrive within 200 ms.
    tokio::time::timeout(std::time::Duration::from_millis(200), rebuild_rx.recv())
        .await
        .expect("rebuild signal should arrive within 200ms")
        .expect("rebuild channel should still be open");
}

#[tokio::test]
async fn update_playlist_sends_obs_rebuild_signal() {
    let state = test_state().await;

    // Seed a playlist directly via the pool (bypass the create path so
    // the signal under test is the update signal).
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'orig', 'u', 'SP-orig')",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let mut rebuild_rx = state.obs_rebuild_tx.subscribe();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/playlists/1")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "ndi_output_name": "SP-renamed"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    tokio::time::timeout(std::time::Duration::from_millis(200), rebuild_rx.recv())
        .await
        .expect("rebuild signal should arrive within 200ms")
        .expect("rebuild channel should still be open");
}

#[tokio::test]
async fn delete_playlist_sends_obs_rebuild_signal() {
    let state = test_state().await;

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'd', 'u', 'SP-d')",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let mut rebuild_rx = state.obs_rebuild_tx.subscribe();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/playlists/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    tokio::time::timeout(std::time::Duration::from_millis(200), rebuild_rx.recv())
        .await
        .expect("rebuild signal should arrive within 200ms")
        .expect("rebuild channel should still be open");
}
