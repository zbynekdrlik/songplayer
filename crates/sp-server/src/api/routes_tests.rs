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
    let resp = app(state)
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
    let resp = app(state)
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

#[tokio::test]
async fn post_seek_returns_204_and_forwards_to_engine() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let (event_tx, _) = broadcast::channel(16);
    let (engine_tx, mut engine_rx) = mpsc::channel(16);
    let (sync_tx, _) = mpsc::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (obs_rebuild_tx, _) = broadcast::channel(4);
    let state = AppState {
        pool,
        event_tx,
        engine_tx,
        obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
        tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
        tool_paths: Arc::new(RwLock::new(None)),
        sync_tx,
        resolume_tx,
        obs_rebuild_tx,
        cache_dir: std::path::PathBuf::from("/tmp/cache"),
        ai_proxy: std::sync::Arc::new(crate::ai::proxy::ProxyManager::new(
            std::path::PathBuf::from("/tmp/cache"),
            crate::ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: std::sync::Arc::new(crate::ai::client::AiClient::new(
            crate::ai::AiSettings::default(),
        )),
        presenter_client: None,
        resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new()),
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    };

    let body = serde_json::json!({"position_ms": 45000});
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/playlists/42/seek")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let cmd = engine_rx
        .recv()
        .await
        .expect("engine must receive a command");
    match cmd {
        crate::EngineCommand::Seek {
            playlist_id,
            position_ms,
        } => {
            assert_eq!(playlist_id, 42);
            assert_eq!(position_ms, 45000);
        }
        other => panic!("unexpected command: {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// PATCH /api/v1/videos/{id} — suppress_resolume_en toggle (for /live setlist)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_video_sets_suppress_resolume_en_true() {
    let state = test_state().await;

    // Seed a playlist + video with the flag off.
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, suppress_resolume_en) \
         VALUES (42, 1, 'yt-abc', 1, 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let pool = state.pool.clone();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/42")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "suppress_resolume_en": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after: i64 = sqlx::query_scalar("SELECT suppress_resolume_en FROM videos WHERE id = 42")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 1, "DB column must flip to 1 after PATCH true");
}

#[tokio::test]
async fn patch_video_sets_suppress_resolume_en_false_then_true_roundtrip() {
    let state = test_state().await;

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, suppress_resolume_en) \
         VALUES (77, 1, 'yt-xyz', 1, 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let pool = state.pool.clone();
    let app = app(state);

    // Turn OFF.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/77")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "suppress_resolume_en": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let off: i64 = sqlx::query_scalar("SELECT suppress_resolume_en FROM videos WHERE id = 77")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(off, 0, "DB column must be 0 after PATCH false");

    // Turn back ON.
    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/77")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "suppress_resolume_en": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let on: i64 = sqlx::query_scalar("SELECT suppress_resolume_en FROM videos WHERE id = 77")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(on, 1, "DB column must flip back to 1 after PATCH true");
}

#[tokio::test]
async fn list_lyrics_songs_exposes_suppress_resolume_en() {
    // The /live setlist UI reads this field from the lyrics-songs response
    // to decide the initial checkbox state. Guarantee it's serialized.
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, suppress_resolume_en) \
         VALUES (1, 1, 'yt-on', 1, 1), (2, 1, 'yt-off', 1, 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/lyrics/songs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(items.len(), 2);

    let on = items
        .iter()
        .find(|v| v["video_id"] == 1)
        .expect("row for video 1");
    let off = items
        .iter()
        .find(|v| v["video_id"] == 2)
        .expect("row for video 2");
    assert_eq!(
        on["suppress_resolume_en"],
        serde_json::Value::Bool(true),
        "video 1 must serialize suppress_resolume_en=true"
    );
    assert_eq!(
        off["suppress_resolume_en"],
        serde_json::Value::Bool(false),
        "video 2 must serialize suppress_resolume_en=false"
    );
}

// Mutation-testing regression: the `Ok(res) if res.rows_affected() == 0`
// guard was replaced with `false` and not caught, meaning a PATCH to a
// non-existent video_id returned 204 instead of 404. Pin the 404 outcome.
#[tokio::test]
async fn patch_video_returns_404_for_missing_video_id() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/99999")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "suppress_resolume_en": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "PATCH on a non-existent video must 404, not 204"
    );
}

// ---------------------------------------------------------------------------
// Gemini audit endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gemini_audit_endpoint_returns_empty_when_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = test_state_with_cache_dir(tmp.path().to_path_buf()).await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/gemini-audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "expected array, got {v}");
    assert_eq!(v.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn gemini_audit_endpoint_applies_video_id_filter() {
    use crate::lyrics::gemini_audit::{GeminiAuditEntry, append};
    let tmp = tempfile::tempdir().unwrap();
    // Seed three entries: two with video_id "wantMe", one with "other".
    let mk = |ts: &str, vid: &str| GeminiAuditEntry {
        timestamp: ts.to_string(),
        video_id: Some(vid.to_string()),
        chunk_idx: Some(0),
        key_idx: 0,
        key_prefix: "AIza".to_string(),
        model: "m".to_string(),
        status: 200,
        duration_ms: 1,
        prompt_tokens: None,
        candidates_tokens: None,
        total_tokens: None,
        error: None,
    };
    append(tmp.path(), &mk("2026-04-23T12:00:00Z", "wantMe"))
        .await
        .unwrap();
    append(tmp.path(), &mk("2026-04-23T12:00:01Z", "other"))
        .await
        .unwrap();
    append(tmp.path(), &mk("2026-04-23T12:00:02Z", "wantMe"))
        .await
        .unwrap();

    let state = test_state_with_cache_dir(tmp.path().to_path_buf()).await;
    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/gemini-audit?video_id=wantMe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 2);
    for e in &entries {
        assert_eq!(e["video_id"], "wantMe");
    }
}

#[tokio::test]
async fn gemini_audit_endpoint_applies_limit() {
    use crate::lyrics::gemini_audit::{GeminiAuditEntry, append};
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..10u32 {
        let entry = GeminiAuditEntry {
            timestamp: format!("2026-04-23T12:00:{i:02}Z"),
            video_id: Some(format!("v{i}")),
            chunk_idx: Some(0),
            key_idx: 0,
            key_prefix: "AIza".to_string(),
            model: "m".to_string(),
            status: 200,
            duration_ms: 1,
            prompt_tokens: None,
            candidates_tokens: None,
            total_tokens: None,
            error: None,
        };
        append(tmp.path(), &entry).await.unwrap();
    }
    let state = test_state_with_cache_dir(tmp.path().to_path_buf()).await;
    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/gemini-audit?limit=3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 3);
    // The truncate keeps the first N after filter — oldest first by file order.
    assert_eq!(entries[0]["video_id"], "v0");
    assert_eq!(entries[2]["video_id"], "v2");
}

#[tokio::test]
async fn resolume_health_endpoint_returns_array() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/resolume/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "response must be a JSON array");
}

/// Verifies the endpoint returns the registered hosts (not an empty Vec).
/// Kills the `get_resolume_health -> Json::from(vec![])` mutant.
#[tokio::test]
async fn resolume_health_endpoint_returns_registered_hosts() {
    let mut state = test_state().await;
    // Replace the empty Arc<ResolumeRegistry> with a populated one.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let mut registry = crate::resolume::ResolumeRegistry::new();
    registry.add_host(1, "10.0.0.99".to_string(), 8090, shutdown_tx.subscribe());
    state.resolume_registry = Arc::new(registry);

    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/resolume/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(arr.len(), 1, "response should contain exactly one host");
    assert_eq!(
        arr[0]["host"].as_str(),
        Some("10.0.0.99"),
        "response must carry the registered host name"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn ndi_health_endpoint_returns_array() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "response must be a JSON array");
}

/// Kills the `get_ndi_health -> Json::from(vec![])` mutant.
/// Mirrors `resolume_health_endpoint_returns_registered_hosts` from PR #54.
#[tokio::test]
async fn ndi_health_endpoint_returns_seeded_pipeline() {
    use crate::playback::ndi_health::{PipelineHealthSnapshot, PlaybackStateLabel};
    let state = test_state().await;
    state.ndi_health_registry.update(PipelineHealthSnapshot {
        playlist_id: 11,
        ndi_name: "SP-test".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 100,
        frames_submitted_last_5s: 30,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        recreate_attempts: 0,
        last_recreate_at_polls: None,
    });
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(arr.len(), 1, "response should contain exactly one pipeline");
    assert_eq!(arr[0]["playlist_id"].as_i64(), Some(11));
    assert_eq!(arr[0]["ndi_name"].as_str(), Some("SP-test"));
    assert_eq!(arr[0]["state"], serde_json::json!("Playing"));
}
