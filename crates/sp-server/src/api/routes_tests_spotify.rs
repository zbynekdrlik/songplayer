//! Tests for the Spotify URL field on `PATCH /api/v1/videos/{id}`.
//!
//! Sibling test file split out of `routes_tests.rs` to keep both files under
//! the airuleset 1000-line cap. Included via the same `#[cfg(test)]
//! #[path = "..."] mod tests_spotify;` pattern as `routes_tests.rs`.

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
    test_state_with_cache_dir(std::path::PathBuf::from("/tmp/cache")).await
}

async fn test_state_with_cache_dir(cache_dir: std::path::PathBuf) -> AppState {
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
        cache_dir: cache_dir.clone(),
        ai_proxy: std::sync::Arc::new(crate::ai::proxy::ProxyManager::new(
            cache_dir,
            crate::ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: std::sync::Arc::new(crate::ai::client::AiClient::new(
            crate::ai::AiSettings::default(),
        )),
        presenter_client: None,
        resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new()),
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    }
}

fn app(state: AppState) -> axum::Router {
    crate::api::router(state, None)
}

async fn insert_test_video(state: &AppState) -> i64 {
    let (playlist_id,): (i64,) = sqlx::query_as(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, is_active) \
         VALUES ('p', 'https://youtube.com/playlist?list=PLtest', 'n', 1) RETURNING id",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let (video_id,): (i64,) = sqlx::query_as(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (?1, 'aaaaaaaaaaa', 't') RETURNING id",
    )
    .bind(playlist_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    video_id
}

#[tokio::test]
async fn patch_video_extracts_spotify_track_id_from_url() {
    let state = test_state().await;
    let video_id = insert_test_video(&state).await;
    let app = app(state.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{video_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "spotify_url": "https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=ab"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let stored = db::models::get_video_spotify_track_id(&state.pool, video_id)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("3n3Ppam7vgaVa1iaRUc9Lp"));
}

#[tokio::test]
async fn patch_video_empty_spotify_url_clears_track_id() {
    let state = test_state().await;
    let video_id = insert_test_video(&state).await;
    // Pre-set a value.
    db::models::set_video_spotify_track_id(&state.pool, video_id, Some("3n3Ppam7vgaVa1iaRUc9Lp"))
        .await
        .unwrap();
    let app = app(state.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{video_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({"spotify_url": ""})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let stored = db::models::get_video_spotify_track_id(&state.pool, video_id)
        .await
        .unwrap();
    assert!(
        stored.is_none(),
        "empty spotify_url must clear track ID to NULL"
    );
}

#[tokio::test]
async fn patch_video_malformed_spotify_url_returns_400() {
    let state = test_state().await;
    let video_id = insert_test_video(&state).await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{video_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({"spotify_url": "not a valid url"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains("spotify_url"),
        "400 body must name the offending field, got: {body_str}"
    );
}

#[tokio::test]
async fn patch_video_empty_body_still_returns_400_after_spotify_field_added() {
    let state = test_state().await;
    let video_id = insert_test_video(&state).await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{video_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
