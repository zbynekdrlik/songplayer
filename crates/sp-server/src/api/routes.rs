//! HTTP request handlers for the REST API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::fs;
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
    pub karaoke_enabled: Option<bool>,
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
    if let Some(karaoke) = body.karaoke_enabled {
        sets.push("karaoke_enabled = ?");
        binds.push(if karaoke { "1" } else { "0" }.to_string());
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

#[derive(Debug, Deserialize)]
pub struct PatchVideoReq {
    #[serde(default)]
    pub suppress_resolume_en: Option<bool>,
    /// Operator-provided lyrics text. When Some(non-empty), the lyrics
    /// worker uses it as the top-priority reference for Gemini alignment,
    /// bypassing yt_subs / description / LRCLIB gather paths. Pass
    /// `Some("")` to clear the override.
    #[serde(default)]
    pub lyrics_override_text: Option<String>,
}

/// Update mutable per-video flags. Currently supports `suppress_resolume_en`
/// and `lyrics_override_text`. Returns 204 on success, 404 if the video
/// id doesn't exist, 400 if the request body has no actionable fields.
pub async fn patch_video(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
    Json(req): Json<PatchVideoReq>,
) -> impl IntoResponse {
    // Require at least one field so empty-body PATCHes are a clear error.
    if req.suppress_resolume_en.is_none() && req.lyrics_override_text.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "request body must include at least one patchable field",
        )
            .into_response();
    }

    // Build a dynamic UPDATE to touch only the columns the caller provided;
    // avoids clobbering unrelated fields across successive PATCHes.
    let mut sets: Vec<&'static str> = Vec::new();
    if req.suppress_resolume_en.is_some() {
        sets.push("suppress_resolume_en = ?");
    }
    if req.lyrics_override_text.is_some() {
        sets.push("lyrics_override_text = ?");
    }
    let sql = format!("UPDATE videos SET {} WHERE id = ?", sets.join(", "));

    let mut q = sqlx::query(&sql);
    if let Some(flag) = req.suppress_resolume_en {
        q = q.bind(flag as i32);
    }
    if let Some(text) = req.lyrics_override_text.as_ref() {
        // Store NULL when the caller passes an empty string so a blank
        // override doesn't silently short-circuit the gather paths.
        if text.trim().is_empty() {
            q = q.bind::<Option<String>>(None);
        } else {
            q = q.bind::<Option<String>>(Some(text.clone()));
        }
    }
    q = q.bind(video_id);

    match q.execute(&state.pool).await {
        Ok(res) if res.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            format!("no video with id {video_id}"),
        )
            .into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Video import endpoint
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ImportVideoReq {
    pub youtube_url: String,
    pub playlist_id: i64,
}

#[derive(Debug, Serialize)]
pub struct ImportVideoResp {
    pub video_id: i64,
    pub youtube_id: String,
    pub title: String,
}

/// Import a YouTube URL into a playlist. Runs `yt-dlp --dump-json` to fetch
/// title/duration, inserts a `videos` row with `normalized=0` (download
/// worker picks it up within 5s), and returns the new id.
pub async fn import_video(
    State(state): State<AppState>,
    Json(req): Json<ImportVideoReq>,
) -> impl IntoResponse {
    use crate::downloader::tools::{extract_youtube_id, fetch_video_metadata};

    // Fast reject obviously non-YouTube URLs before shelling out.
    if extract_youtube_id(&req.youtube_url).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "URL does not look like a YouTube video link",
        )
            .into_response();
    }

    let ytdlp_path = {
        let guard = state.tool_paths.read().await;
        match guard.as_ref() {
            Some(tp) => tp.ytdlp.clone(),
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "yt-dlp not ready yet on this server",
                )
                    .into_response();
            }
        }
    };

    let meta = match fetch_video_metadata(&ytdlp_path, &req.youtube_url).await {
        Ok(m) => m,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("yt-dlp failed: {e}")).into_response();
        }
    };

    let row = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, duration_ms, normalized) \
         VALUES (?, ?, ?, ?, 0) \
         ON CONFLICT(playlist_id, youtube_id) DO UPDATE SET title = excluded.title \
         RETURNING id",
    )
    .bind(req.playlist_id)
    .bind(&meta.youtube_id)
    .bind(&meta.title)
    .bind(meta.duration_ms.map(|ms| ms as i64))
    .fetch_one(&state.pool)
    .await;

    match row {
        Ok(r) => {
            let id: i64 = r.get(0);
            (
                StatusCode::CREATED,
                Json(ImportVideoResp {
                    video_id: id,
                    youtube_id: meta.youtube_id,
                    title: meta.title,
                }),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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

#[derive(Debug, serde::Deserialize)]
pub struct SeekReq {
    pub position_ms: u64,
}

/// Jump playback of the given playlist to `position_ms`. Returns 204
/// No Content on success. Always-ok for valid playlist ids — the pipeline
/// drops the command when no song is loaded.
pub async fn post_seek(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<SeekReq>,
) -> impl IntoResponse {
    match state
        .engine_tx
        .send(EngineCommand::Seek {
            playlist_id,
            position_ms: req.position_ms,
        })
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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

/// GET /api/v1/resolume/health
///
/// Returns a per-host snapshot of the Resolume push chain health.
pub async fn get_resolume_health(
    State(state): State<AppState>,
) -> Json<Vec<crate::resolume::HostHealthSnapshot>> {
    Json(state.resolume_registry.health_snapshots())
}

/// GET /api/v1/ndi/health — return per-pipeline NDI delivery health.
/// Empty `[]` if no pipelines have reported a heartbeat yet.
pub async fn get_ndi_health(
    State(state): State<AppState>,
) -> Json<Vec<crate::playback::ndi_health::PipelineHealthSnapshot>> {
    Json(state.ndi_health_registry.snapshots())
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
// Lyrics endpoints
// ---------------------------------------------------------------------------

/// GET /api/v1/videos/:id/lyrics
///
/// Returns the cached lyrics JSON for a video. 404 if not available.
#[cfg_attr(test, mutants::skip)]
pub async fn get_video_lyrics(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    // Query the video to check has_lyrics and get youtube_id.
    let result = sqlx::query("SELECT youtube_id, has_lyrics FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(&state.pool)
        .await;

    let row = match result {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("get_video_lyrics db error: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let has_lyrics: i32 = row.get("has_lyrics");
    if has_lyrics == 0 {
        return StatusCode::NOT_FOUND.into_response();
    }

    let youtube_id: String = row.get("youtube_id");
    let lyrics_path = state.cache_dir.join(format!("{youtube_id}_lyrics.json"));

    match fs::read_to_string(&lyrics_path).await {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(json) => Json(json).into_response(),
            Err(e) => {
                warn!("get_video_lyrics parse error for {youtube_id}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        Err(e) => {
            warn!("get_video_lyrics read error for {youtube_id}: {e}");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// POST /api/v1/videos/:id/lyrics/reprocess
///
/// Re-queues a video for lyrics processing.
pub async fn reprocess_video_lyrics(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    match crate::db::models::reset_video_lyrics(&state.pool, video_id).await {
        Ok(()) => Json(serde_json::json!({"status": "queued"})).into_response(),
        Err(e) => {
            warn!("reprocess_video_lyrics error for video {video_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// GET /api/v1/lyrics/status
///
/// Returns the lyrics processing queue status across all active playlists.
pub async fn get_lyrics_status(State(state): State<AppState>) -> impl IntoResponse {
    match crate::db::models::get_lyrics_status(&state.pool).await {
        Ok((total, processed, pending)) => Json(serde_json::json!({
            "total": total,
            "processed": processed,
            "pending": pending,
        }))
        .into_response(),
        Err(e) => {
            warn!("get_lyrics_status error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Spotify integration
// ---------------------------------------------------------------------------

/// Extract a Spotify track ID from any of:
/// - canonical URL: `https://open.spotify.com/track/<id>` (with or without `?si=...`, with or without trailing `/`)
/// - localized URL: `https://open.spotify.com/intl-cz/track/<id>?si=...`
/// - bare 22-char alphanumeric ID
///
/// Returns `Err` for empty input, missing `/track/` segment, or IDs that
/// don't match Spotify's 22-char base62 shape.
pub(crate) fn parse_spotify_track_id(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("spotify_url is empty");
    }
    let candidate = if let Some(idx) = trimmed.find("/track/") {
        let after = &trimmed[idx + "/track/".len()..];
        let cut = after
            .find(|c: char| c == '?' || c == '/')
            .unwrap_or(after.len());
        &after[..cut]
    } else {
        trimmed
    };
    if candidate.len() == 22 && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(candidate.to_string())
    } else {
        Err("not a valid Spotify track ID (must be 22 alphanumeric chars)")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod parse_spotify_tests {
    use super::parse_spotify_track_id;

    #[test]
    fn extracts_id_from_canonical_url() {
        let id = parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp")
            .unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_url_with_si_query() {
        let id =
            parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=abcd")
                .unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_url_with_trailing_slash() {
        let id = parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp/")
            .unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_intl_url() {
        let id = parse_spotify_track_id(
            "https://open.spotify.com/intl-cz/track/3n3Ppam7vgaVa1iaRUc9Lp?si=xyz",
        )
        .unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn accepts_bare_track_id() {
        let id = parse_spotify_track_id("3n3Ppam7vgaVa1iaRUc9Lp").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_spotify_track_id("").is_err());
    }

    #[test]
    fn rejects_whitespace_only() {
        assert!(parse_spotify_track_id("   ").is_err());
    }

    #[test]
    fn rejects_url_without_track_path() {
        assert!(
            parse_spotify_track_id("https://open.spotify.com/album/3n3Ppam7vgaVa1iaRUc9Lp")
                .is_err()
        );
    }

    #[test]
    fn rejects_id_too_short() {
        assert!(parse_spotify_track_id("3n3Ppam7vga").is_err());
    }

    #[test]
    fn rejects_id_too_long() {
        assert!(parse_spotify_track_id("3n3Ppam7vgaVa1iaRUc9LpXXX").is_err());
    }

    #[test]
    fn rejects_id_with_invalid_chars() {
        assert!(parse_spotify_track_id("3n3Ppam7vga!a1iaRUc9Lp").is_err());
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
