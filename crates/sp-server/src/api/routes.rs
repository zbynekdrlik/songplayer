//! HTTP request handlers for the REST API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;

use sp_core::playback::PlaybackMode;

use crate::{AppState, EngineCommand, SyncRequest};

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
    pub playback_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistRequest {
    pub name: Option<String>,
    pub youtube_url: Option<String>,
    pub ndi_output_name: Option<String>,
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
    /// Playlists whose NDI source is matched in the current OBS program
    /// scene by [`scene::check_scene_items`]. Populated only after the
    /// `ndi_sources` map has been rebuilt from the DB + OBS input
    /// settings — an empty list here on a known-good scene is the
    /// symptom of issue #11 and is what the post-deploy tests assert
    /// against.
    pub active_playlist_ids: Vec<i64>,
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
        "SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at
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
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode)
         VALUES (?, ?, ?, ?)
         RETURNING id, name, youtube_url, ndi_output_name, playback_mode, is_active",
    )
    .bind(&body.name)
    .bind(&body.youtube_url)
    .bind(ndi)
    .bind(mode)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => {
            // Trigger a scene-detection rebuild so the new playlist can be
            // matched against OBS NDI inputs immediately.
            let _ = state.obs_rebuild_tx.send(());
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": row.get::<i64, _>("id"),
                    "name": row.get::<String, _>("name"),
                    "youtube_url": row.get::<String, _>("youtube_url"),
                    "ndi_output_name": row.get::<String, _>("ndi_output_name"),
                    "playback_mode": row.get::<String, _>("playback_mode"),
                    "is_active": row.get::<i32, _>("is_active") != 0,
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("create_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let result = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at
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
                let _ = state.obs_rebuild_tx.send(());
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
                let _ = state.obs_rebuild_tx.send(());
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
        Ok(Some(row)) => {
            let youtube_url: String = row.get("youtube_url");
            let req = SyncRequest {
                playlist_id: id,
                youtube_url,
            };
            match state.sync_tx.send(req).await {
                Ok(_) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"message": "sync queued"})),
                )
                    .into_response(),
                Err(e) => {
                    warn!("failed to queue sync for playlist {id}: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
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

pub async fn previous(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(EngineCommand::Previous { playlist_id })
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

    let mut active_playlist_ids: Vec<i64> = obs.active_playlist_ids.iter().copied().collect();
    active_playlist_ids.sort_unstable();

    Json(StatusResponse {
        version: sp_core::config::VERSION.to_string(),
        obs_connected: obs.connected,
        active_scene: obs.current_scene.clone(),
        active_playlist_ids,
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
#[path = "routes_tests.rs"]
mod tests;
