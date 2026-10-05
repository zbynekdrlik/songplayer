//! HTTP request handlers for the REST API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::fs;
use tracing::{error, warn};

use super::routes_mode::{MODE_ORDER, requested_mode};
use crate::metadata::manual::refused_title;
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
    /// #221 L4b: the playlists on air — SP-program's playlist alone (B4 step
    /// 6), none for "OBS manuál" (`routes_status`). `active_scene` is
    /// SongPlayer's own program scene name (the one resolver).
    pub active_playlist_ids: Vec<i64>,
    pub tools: ToolsStatusResponse,
    pub playlist_count: i64,
    /// LAN `sp.local` URL the dashboard is reachable at without internet (#51):
    /// `Some("http://sp.local:8920")` while advertised, else `None` (a missing
    /// `Option` deserializes to `None`, so the mock / older clients stay ok).
    pub lan_url: Option<String>,
    /// The box's routable LAN IPv4 as a raw fallback for the dashboard (#51).
    pub lan_ip: Option<String>,
    /// #178: the H.264 encoder the live preview stream selected for this
    /// process (`h264_nvenc` / `h264_qsv` / `h264_amf` / `libx264`), or `None`
    /// until a preview child has been spawned. A missing key deserializes to
    /// `None` (older clients / the mock stay ok).
    #[serde(default)]
    pub preview_encoder: Option<String>,
    /// #196: seconds since this SongPlayer process started. The post-deploy E2E
    /// job reads it to SKIP restarting a process the Deploy job started < 10 min
    /// ago (item 6 — one restart per push). A missing key deserializes to `0`.
    #[serde(default)]
    pub uptime_s: u64,
    /// #203: the OS-level containment applied to every heavy background child
    /// (stems / lyrics / dub) — the live `heavy_cpu_cap_pct` +
    /// `heavy_cpu_affinity_mask` settings resolved against the box's core count,
    /// plus SongPlayer's own priority class. A missing key deserializes to the
    /// zero value (older clients / the mock stay ok).
    #[serde(default)]
    pub heavy_containment: HeavyContainmentStatus,
    /// #207: box-wide commit / pagefile snapshot in MB (Windows only; `None`
    /// off Windows or on a read failure — a missing key deserializes to `None`).
    #[serde(default)]
    pub commit: Option<crate::lyrics::host_commit::HostCommitStatus>,
    /// #136: the metadata provider chain — repair-queue size + per-provider
    /// health (`api::metadata::status_block`). Missing key → default.
    #[serde(default)]
    pub metadata: crate::metadata::health::MetadataStatus,
    /// #223 S3b: the `video_hw_decode` setting and what the hardware decode
    /// path did (`playback::video_decode::status`). Missing key → default.
    #[serde(default)]
    pub video_decode: crate::playback::video_decode::VideoDecodeStatus,
}

// #136 / #223 S3b: moved to `routes_status` for the 1000-line cap.
pub use super::routes_status::{HeavyContainmentStatus, ToolsStatusResponse};

// ---------------------------------------------------------------------------
// Playlist endpoints
// ---------------------------------------------------------------------------

pub async fn list_playlists(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at, kind
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
                        // #194 r3c: expose `kind` so the shared PlaylistPicker can
                        // filter by it (Live shows only the `custom` playlist) —
                        // no hardcoded `name == "ytlive"` lookup. NULL-safe: an
                        // old row with no kind reads as the default `youtube`.
                        "kind": r.get::<Option<String>, _>("kind")
                            .unwrap_or_else(|| "youtube".to_string()),
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
    // #225 unit 2: a known mode only, by its canonical name (`routes_mode`).
    let mode = match requested_mode(None, body.playback_mode.as_deref()) {
        Ok(mode) => mode.unwrap_or_default(),
        Err(refused) => return refused.into_response(),
    };

    let result = sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode)
         VALUES (?, ?, ?, ?)
         RETURNING id, name, youtube_url, ndi_output_name, playback_mode, is_active",
    )
    .bind(&body.name)
    .bind(&body.youtube_url)
    .bind(ndi)
    .bind(mode.as_str())
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => {
            // Extract every field into owned values so the `SqliteRow` is not
            // held across the `engine_tx.send().await` below.
            let id = row.get::<i64, _>("id");
            let name = row.get::<String, _>("name");
            let youtube_url = row.get::<String, _>("youtube_url");
            let ndi_output_name = row.get::<String, _>("ndi_output_name");
            let playback_mode = row.get::<String, _>("playback_mode");
            let is_active = row.get::<i32, _>("is_active") != 0;
            drop(row);

            // Rebuild the NDI source map so the new playlist is matched
            // against OBS NDI inputs immediately (the #196 self-check).
            let _ = state.obs_rebuild_tx.send(());
            // #132: register a playback pipeline for the new playlist so the
            // playback authority can start it without a restart. The engine
            // reconciles from the DB (creates only when active + non-empty NDI).
            // GUARANTEED delivery (`.send().await`, not `try_send`): a dropped
            // command would leave the playlist unplayable until a restart — the
            // exact bug this fixes — since nothing else creates it until it goes
            // on air (the authority's ON; `apply_event` only warns). The engine
            // drains `engine_rx` on an independent task, so this never deadlocks.
            let _ = state
                .engine_tx
                .send(EngineCommand::EnsurePipeline { playlist_id: id })
                .await;
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": id,
                    "name": name,
                    "youtube_url": youtube_url,
                    "ndi_output_name": ndi_output_name,
                    "playback_mode": playback_mode,
                    "is_active": is_active,
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
    // #225 unit 2: the row is the mode's one truth — a known mode only, by
    // its canonical name, and the engine is told it (`routes_mode`).
    let mode = match requested_mode(Some(id), body.playback_mode.as_deref()) {
        Ok(mode) => mode,
        Err(refused) => return refused.into_response(),
    };
    if let Some(mode) = mode {
        sets.push("playback_mode = ?");
        binds.push(mode.as_str().to_string());
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

    let _order = MODE_ORDER.lock().await; // the row, then the engine (#225)
    match query.execute(&state.pool).await {
        Ok(result) => {
            if result.rows_affected() == 0 {
                StatusCode::NOT_FOUND.into_response()
            } else {
                if let Some(mode) = mode {
                    super::routes_mode::tell_engine(&state.engine_tx, id, mode).await;
                }
                let _ = state.obs_rebuild_tx.send(());
                // #132: reconcile the playback pipeline with the update.
                // Deactivation tears the pipeline down; every other update
                // (activation, NDI-name set, rename) ensures it — the ensure
                // handler is idempotent and a no-op when the playlist is
                // inactive / has no NDI name / already has a pipeline. Guaranteed
                // delivery (`.send().await`), as in `create_playlist`.
                let cmd = if body.is_active == Some(false) {
                    EngineCommand::RemovePipeline { playlist_id: id }
                } else {
                    EngineCommand::EnsurePipeline { playlist_id: id }
                };
                let _ = state.engine_tx.send(cmd).await;
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(e) => {
            error!(id, %e, "update_playlist: the row was not written");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // #225 unit 2: never between a mode change's write and its tell.
    let _order = MODE_ORDER.lock().await;
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
                // #132: tear down the deleted playlist's pipeline symmetrically.
                // Guaranteed delivery (`.send().await`), as in `create_playlist`.
                let _ = state
                    .engine_tx
                    .send(EngineCommand::RemovePipeline { playlist_id: id })
                    .await;
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

// `list_videos` moved to `api/videos.rs` (#177) so the videos payload can carry
// the additive per-song `stems_state` marker without pushing this file over the
// 1000-line cap.

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
    /// Operator correction of the wall song title (#136 T1). Sanitized
    /// through the same central choke point as an ingested title; a
    /// whitespace-only value is rejected 400 (a blank song has no
    /// meaningful wall display).
    #[serde(default)]
    pub song: Option<String>,
    /// Operator correction of the wall artist (#136 T1). Sanitized like
    /// `song`; an empty value clears the column to NULL (some songs have no
    /// artist), mirroring the `lyrics_override_text` empty->NULL convention.
    #[serde(default)]
    pub artist: Option<String>,
}

/// Update mutable per-video flags. Supports `suppress_resolume_en`,
/// `lyrics_override_text`, and the `song` / `artist` metadata correction
/// levers (#136 T1; a correction is final and belongs to the video: every row
/// of it, files renamed, `metadata::manual::apply_to_video`). Returns 204 on
/// success, 404 if the video id doesn't exist, 400 if the request body has no
/// actionable fields, a whitespace-only `song`, or an `artist` alone for a
/// video with no song yet (`metadata::manual::refused_title`).
pub async fn patch_video(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
    Json(req): Json<PatchVideoReq>,
) -> impl IntoResponse {
    // Sanitize operator-provided metadata through the SAME central choke
    // point (`metadata::sanitize::strip_emoji`) that `metadata::get_metadata`
    // applies to every ingested provider/fallback title, so a manual
    // correction is cleaned identically (emoji / high-plane junk stripped,
    // whitespace collapsed and trimmed). `strip_emoji` already returns a
    // trimmed, whitespace-collapsed string, so `is_empty()` == whitespace-only.
    let song = req
        .song
        .as_ref()
        .map(|s| crate::metadata::sanitize::strip_emoji(s));
    let artist = req
        .artist
        .as_ref()
        .map(|a| crate::metadata::sanitize::strip_emoji(a));

    // A blank song; an artist alone for a video with no song (#136 review 2).
    let refused = refused_title(&state.pool, video_id, song.as_deref(), artist.is_some());
    match refused.await {
        Ok(None) => {}
        Ok(Some(why)) => return (StatusCode::BAD_REQUEST, why).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }

    // Require at least one field so empty-body PATCHes are a clear error.
    if req.suppress_resolume_en.is_none()
        && req.lyrics_override_text.is_none()
        && song.is_none()
        && artist.is_none()
    {
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
    if song.is_some() {
        sets.push("song = ?");
    }
    if artist.is_some() {
        sets.push("artist = ?");
    }
    // #136: a title correction is final — the row leaves the metadata repair
    // queue (`metadata::health::REPAIR_QUEUE_WHERE`), which would write over it.
    let corrects_title = song.is_some() || artist.is_some();
    if corrects_title {
        sets.push("gemini_failed = 0, metadata_source = 'manual'");
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
    if let Some(s) = song.as_ref() {
        // Validated non-empty above.
        q = q.bind::<Option<String>>(Some(s.clone()));
    }
    if let Some(a) = artist.as_ref() {
        // Empty artist clears the column (some songs legitimately have none).
        if a.is_empty() {
            q = q.bind::<Option<String>>(None);
        } else {
            q = q.bind::<Option<String>>(Some(a.clone()));
        }
    }
    q = q.bind(video_id);

    // #136: a repair in flight re-checks the queue and writes under the
    // song-files lock, so a correction waits for it and is never overwritten.
    let _files = if corrects_title {
        Some(crate::downloader::cache::SONG_FILES.lock().await)
    } else {
        None
    };
    match q.execute(&state.pool).await {
        Ok(res) if res.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            format!("no video with id {video_id}"),
        )
            .into_response(),
        Ok(_) if corrects_title => {
            let spread =
                crate::metadata::manual::apply_to_video(&state.pool, &state.cache_dir, video_id);
            match spread.await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
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
/// worker picks it up within 5s), and returns the new id. The shared core lives
/// in `api/routes_import.rs` (also used by the Dabing import, #180) so this file
/// stays under the 1000-line cap.
pub async fn import_video(
    State(state): State<AppState>,
    Json(req): Json<ImportVideoReq>,
) -> impl IntoResponse {
    match crate::api::routes_import::import_video_core(&state, &req.youtube_url, req.playlist_id)
        .await
    {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err((status, msg)) => (status, msg).into_response(),
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

// #225 unit 2: the mode route is in `api/routes_mode.rs`; #194: the seek
// route in `api/routes_seek.rs` (the `/api/v1/playback/{id}/…` family).

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
    // #144: each status copied out at once, no lock held across the awaits below.
    let obs_connected = state.obs_state.read().await.connected;
    let tools = state.tools_status.read().await.clone();
    let lan = state.lan_status.read().await.clone();
    let playlist_count = sqlx::query("SELECT COUNT(*) AS c FROM playlists")
        .fetch_one(&state.pool)
        .await
        .map(|r| r.get::<i64, _>("c"))
        .unwrap_or(0);

    let (active_scene, active_playlist_ids) =
        super::routes_status::on_air_fields(&state.program_bus);

    // #203/#207/#207r3c: resolve the live containment; purge delay/alloc mode/reserve_gib/max_ws (#147 r9) are internal-only, not surfaced here.
    let heavy_cap = crate::db::models::get_setting(&state.pool, "heavy_cpu_cap_pct")
        .await
        .ok()
        .flatten();
    let heavy_mask = crate::db::models::get_setting(&state.pool, "heavy_cpu_affinity_mask")
        .await
        .ok()
        .flatten();
    let containment = crate::lyrics::heavy_containment::containment_from_settings(
        heavy_cap.as_deref(),
        heavy_mask.as_deref(),
        None,
        None,
        None,
        None,
        crate::lyrics::heavy_slot::logical_cores(),
    );

    Json(StatusResponse {
        version: sp_core::config::VERSION.to_string(),
        obs_connected,
        active_scene,
        active_playlist_ids,
        tools: ToolsStatusResponse {
            ytdlp_available: tools.ytdlp_available,
            ffmpeg_available: tools.ffmpeg_available,
            ytdlp_version: tools.ytdlp_version,
            js_runtime_ok: tools.js_runtime_ok,
            deno_version: tools.deno_version,
        },
        playlist_count,
        lan_url: lan.lan_url,
        lan_ip: lan.lan_ip,
        preview_encoder: crate::playback::preview::preview_encoder::chosen_encoder(),
        uptime_s: crate::process_start::uptime_secs(),
        heavy_containment: HeavyContainmentStatus {
            cap_pct: containment.cpu_cap_pct,
            affinity_mask: crate::lyrics::heavy_containment::affinity_mask_hex(
                containment.affinity_mask,
            ),
            priority_class: crate::process_start::priority_class_label().to_string(),
        },
        commit: crate::lyrics::host_commit::read_status(),
        metadata: super::metadata::status_block(&state).await,
        video_decode: crate::playback::video_decode::status(),
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

/// Body for `POST /api/v1/ndi/burn` (#151).
#[derive(Debug, Deserialize)]
pub struct SetBurnRequest {
    /// The NDI output name (e.g. `"SP-fast"`) to toggle.
    pub output: String,
    /// Turn the burn-id QR overlay on (`true`) or off (`false`).
    pub on: bool,
}

/// POST /api/v1/ndi/burn — toggle the runtime burn-id QR overlay for one NDI
/// output (#151). `204` on success; `404` if the output is unknown; `409`
/// ("pacing disabled") when the output exists but `genlock_pacing` is off (the
/// burn is only painted on the paced path — the fleet's TEST mode runs with
/// pacing on). Default OFF, never persisted.
pub async fn set_ndi_burn(
    State(state): State<AppState>,
    Json(body): Json<SetBurnRequest>,
) -> impl IntoResponse {
    use crate::playback::ndi_burn::BurnSetResult;
    match state.ndi_burn_registry.set(&body.output, body.on) {
        BurnSetResult::Ok => StatusCode::NO_CONTENT.into_response(),
        BurnSetResult::NotFound => StatusCode::NOT_FOUND.into_response(),
        BurnSetResult::PacingDisabled => (StatusCode::CONFLICT, "pacing disabled").into_response(),
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
// Lyrics endpoints
// ---------------------------------------------------------------------------

/// GET /api/v1/videos/:id/lyrics
///
/// Returns the cached lyrics JSON for a video. 204 when the song simply has no
/// lyrics (a normal state — a 404 would log a browser console error on every
/// idle Player, #194), 404 for an unknown video id or a missing sidecar.
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
        return StatusCode::NO_CONTENT.into_response();
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "routes_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "routes_tests_reference.rs"]
mod tests_reference;

#[cfg(test)]
#[path = "routes_tests_translation.rs"]
mod tests_translation;

#[cfg(test)]
#[path = "routes_tests_clock.rs"]
mod tests_clock;

#[cfg(test)]
#[path = "routes_tests_pacing.rs"]
mod tests_pacing;

#[cfg(test)]
#[path = "routes_tests_burn.rs"]
mod tests_burn;

#[cfg(test)]
#[path = "routes_tests_runtime_pipeline.rs"]
mod tests_runtime_pipeline;

#[cfg(test)]
#[path = "routes_tests_patch_metadata.rs"]
mod tests_patch_metadata;

#[path = "routes_tests_lyrics_fetch.rs"]
#[cfg(test)]
mod tests_lyrics_fetch;
