//! HTTP request handlers for the REST API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;

use sp_core::playback::PlaybackMode;

use crate::{AppState, EngineCommand};

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistRequest {
    pub name: String,
    pub youtube_url: String,
    #[serde(default)]
    pub ndi_output_name: Option<String>,
    #[serde(default)]
    pub obs_text_source: Option<String>,
    #[serde(default)]
    pub playback_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistRequest {
    pub name: Option<String>,
    pub youtube_url: Option<String>,
    pub ndi_output_name: Option<String>,
    pub obs_text_source: Option<String>,
    pub playback_mode: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    #[serde(flatten)]
    pub settings: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct AddResolumeHostRequest {
    pub label: String,
    pub host: String,
    #[serde(default = "default_resolume_port")]
    pub port: u16,
}

fn default_resolume_port() -> u16 {
    8090
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub obs_connected: bool,
    pub active_scene: Option<String>,
    pub tools: ToolsStatusResponse,
    pub playlist_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolsStatusResponse {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Playlist endpoints
// ---------------------------------------------------------------------------

pub async fn list_playlists(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, obs_text_source, playback_mode, is_active, created_at, updated_at
         FROM playlists ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(rows) => {
            let playlists: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.get::<i64, _>("id"),
                        "name": r.get::<String, _>("name"),
                        "youtube_url": r.get::<String, _>("youtube_url"),
                        "ndi_output_name": r.get::<String, _>("ndi_output_name"),
                        "obs_text_source": r.get::<Option<String>, _>("obs_text_source"),
                        "playback_mode": r.get::<String, _>("playback_mode"),
                        "is_active": r.get::<i32, _>("is_active") != 0,
                        "created_at": r.get::<String, _>("created_at"),
                        "updated_at": r.get::<String, _>("updated_at"),
                    })
                })
                .collect();
            Json(playlists).into_response()
        }
        Err(e) => {
            warn!("list_playlists error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn create_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreatePlaylistRequest>,
) -> impl IntoResponse {
    let ndi = body.ndi_output_name.as_deref().unwrap_or("");
    let mode = body.playback_mode.as_deref().unwrap_or("continuous");

    let result = sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, obs_text_source, playback_mode)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id, name, youtube_url, ndi_output_name, obs_text_source, playback_mode, is_active",
    )
    .bind(&body.name)
    .bind(&body.youtube_url)
    .bind(ndi)
    .bind(&body.obs_text_source)
    .bind(mode)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": row.get::<i64, _>("id"),
                "name": row.get::<String, _>("name"),
                "youtube_url": row.get::<String, _>("youtube_url"),
                "ndi_output_name": row.get::<String, _>("ndi_output_name"),
                "obs_text_source": row.get::<Option<String>, _>("obs_text_source"),
                "playback_mode": row.get::<String, _>("playback_mode"),
                "is_active": row.get::<i32, _>("is_active") != 0,
            })),
        )
            .into_response(),
        Err(e) => {
            warn!("create_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let result = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, obs_text_source, playback_mode, is_active, created_at, updated_at
         FROM playlists WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await;

    match result {
        Ok(Some(row)) => Json(serde_json::json!({
            "id": row.get::<i64, _>("id"),
            "name": row.get::<String, _>("name"),
            "youtube_url": row.get::<String, _>("youtube_url"),
            "ndi_output_name": row.get::<String, _>("ndi_output_name"),
            "obs_text_source": row.get::<Option<String>, _>("obs_text_source"),
            "playback_mode": row.get::<String, _>("playback_mode"),
            "is_active": row.get::<i32, _>("is_active") != 0,
            "created_at": row.get::<String, _>("created_at"),
            "updated_at": row.get::<String, _>("updated_at"),
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("get_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn update_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdatePlaylistRequest>,
) -> impl IntoResponse {
    // Build dynamic update query.
    let mut sets = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref name) = body.name {
        sets.push("name = ?");
        binds.push(name.clone());
    }
    if let Some(ref url) = body.youtube_url {
        sets.push("youtube_url = ?");
        binds.push(url.clone());
    }
    if let Some(ref ndi) = body.ndi_output_name {
        sets.push("ndi_output_name = ?");
        binds.push(ndi.clone());
    }
    if let Some(ref obs) = body.obs_text_source {
        sets.push("obs_text_source = ?");
        binds.push(obs.clone());
    }
    if let Some(ref mode) = body.playback_mode {
        sets.push("playback_mode = ?");
        binds.push(mode.clone());
    }
    if let Some(active) = body.is_active {
        sets.push("is_active = ?");
        binds.push(if active { "1" } else { "0" }.to_string());
    }

    if sets.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    sets.push("updated_at = datetime('now')");
    let sql = format!("UPDATE playlists SET {} WHERE id = ?", sets.join(", "));

    let mut query = sqlx::query(&sql);
    for val in &binds {
        query = query.bind(val);
    }
    query = query.bind(id);

    match query.execute(&state.pool).await {
        Ok(result) => {
            if result.rows_affected() == 0 {
                StatusCode::NOT_FOUND.into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(e) => {
            warn!("update_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match sqlx::query("DELETE FROM playlists WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        Ok(result) => {
            if result.rows_affected() == 0 {
                StatusCode::NOT_FOUND.into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(e) => {
            warn!("delete_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn sync_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // Look up the playlist URL.
    let row = sqlx::query("SELECT youtube_url FROM playlists WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await;

    match row {
        Ok(Some(_row)) => {
            // Playlist sync would be triggered via a channel to the playlist sync worker.
            // For now, return accepted.
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"message": "sync queued"})),
            )
                .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("sync_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn list_videos(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    match crate::db::models::get_videos_for_playlist(&state.pool, id).await {
        Ok(videos) => Json(videos).into_response(),
        Err(e) => {
            warn!("list_videos error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Playback endpoints
// ---------------------------------------------------------------------------

pub async fn play(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(EngineCommand::Play { playlist_id })
        .await;
    StatusCode::NO_CONTENT
}

pub async fn pause(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(EngineCommand::Pause { playlist_id })
        .await;
    StatusCode::NO_CONTENT
}

pub async fn skip(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(EngineCommand::Skip { playlist_id })
        .await;
    StatusCode::NO_CONTENT
}

pub async fn set_mode(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(body): Json<SetModeRequest>,
) -> impl IntoResponse {
    let mode = PlaybackMode::from_str_lossy(&body.mode);
    let _ = state
        .engine_tx
        .send(EngineCommand::SetMode { playlist_id, mode })
        .await;
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// Settings endpoints
// ---------------------------------------------------------------------------

pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
        .fetch_all(&state.pool)
        .await;

    match rows {
        Ok(rows) => {
            let mut map = serde_json::Map::new();
            for r in &rows {
                let key: String = r.get("key");
                let value: String = r.get("value");
                map.insert(key, serde_json::Value::String(value));
            }
            Json(serde_json::Value::Object(map)).into_response()
        }
        Err(e) => {
            warn!("get_settings error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn update_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    for (key, value) in &body.settings {
        if let Err(e) = crate::db::models::set_setting(&state.pool, key, value).await {
            warn!("update_settings error for key {key}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// Status endpoint
// ---------------------------------------------------------------------------

pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let obs = state.obs_state.read().await;
    let tools = state.tools_status.read().await;

    let playlist_count = sqlx::query("SELECT COUNT(*) AS c FROM playlists")
        .fetch_one(&state.pool)
        .await
        .map(|r| r.get::<i64, _>("c"))
        .unwrap_or(0);

    Json(StatusResponse {
        version: sp_core::config::VERSION.to_string(),
        obs_connected: obs.connected,
        active_scene: obs.current_scene.clone(),
        tools: ToolsStatusResponse {
            ytdlp_available: tools.ytdlp_available,
            ffmpeg_available: tools.ffmpeg_available,
            ytdlp_version: tools.ytdlp_version.clone(),
        },
        playlist_count,
    })
}

// ---------------------------------------------------------------------------
// Resolume endpoints
// ---------------------------------------------------------------------------

pub async fn list_resolume_hosts(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query(
        "SELECT id, label, host, port, is_enabled, created_at FROM resolume_hosts ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(rows) => {
            let hosts: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.get::<i64, _>("id"),
                        "label": r.get::<String, _>("label"),
                        "host": r.get::<String, _>("host"),
                        "port": r.get::<i32, _>("port"),
                        "is_enabled": r.get::<i32, _>("is_enabled") != 0,
                        "created_at": r.get::<String, _>("created_at"),
                    })
                })
                .collect();
            Json(hosts).into_response()
        }
        Err(e) => {
            warn!("list_resolume_hosts error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn add_resolume_host(
    State(state): State<AppState>,
    Json(body): Json<AddResolumeHostRequest>,
) -> impl IntoResponse {
    let result = sqlx::query(
        "INSERT INTO resolume_hosts (label, host, port) VALUES (?, ?, ?) RETURNING id, label, host, port, is_enabled",
    )
    .bind(&body.label)
    .bind(&body.host)
    .bind(body.port as i32)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": row.get::<i64, _>("id"),
                "label": row.get::<String, _>("label"),
                "host": row.get::<String, _>("host"),
                "port": row.get::<i32, _>("port"),
                "is_enabled": row.get::<i32, _>("is_enabled") != 0,
            })),
        )
            .into_response(),
        Err(e) => {
            warn!("add_resolume_host error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_resolume_host(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match sqlx::query("DELETE FROM resolume_hosts WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        Ok(result) => {
            if result.rows_affected() == 0 {
                StatusCode::NOT_FOUND.into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(e) => {
            warn!("delete_resolume_host error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tokio::sync::{RwLock, broadcast, mpsc};
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let (engine_tx, _) = mpsc::channel(16);
        AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
        }
    }

    fn app(state: AppState) -> axum::Router {
        crate::api::router(state)
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
                    .uri(&format!("/api/v1/resolume/hosts/{host_id}"))
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
    }
}
