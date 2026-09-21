//! Dabing section API (#180 — dubbing D1). Registered in `api/mod.rs`.
//!
//! - `GET  /api/v1/dabing`               → `{playlist_id, videos:[DubRow…]}`
//! - `POST /api/v1/dabing/import {url}`   → import the URL into the seeded
//!   Dabing playlist (shared core, cookie-gated) then mark it dub-requested.
//! - `PATCH /api/v1/videos/{id}/dub {requested}`    → per-video dub toggle.
//! - `PATCH /api/v1/videos/{id}/dub-mix {ratio}`    → per-video mixer blend.
//!
//! The dub CHAIN (stems/transcript/translation/synth) is D3/D4 — this lane only
//! records the request + serves the state for display.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;
use crate::db::models_dabing::{self, DubRow};

/// `POST /api/v1/dabing/import` body — a bare YouTube URL.
#[derive(Debug, Deserialize)]
pub struct DabingImportReq {
    pub url: String,
}

/// `PATCH /api/v1/videos/{id}/dub` body — the toggle state.
#[derive(Debug, Deserialize)]
pub struct DubToggleReq {
    pub requested: bool,
}

/// `PATCH /api/v1/videos/{id}/dub-mix` body — the mixer blend ratio (0.0..=1.0).
#[derive(Debug, Deserialize)]
pub struct DubMixReq {
    pub ratio: f64,
}

/// `GET /api/v1/dabing` response.
#[derive(Debug, Serialize)]
pub struct DabingListResp {
    pub playlist_id: Option<i64>,
    pub videos: Vec<DubRow>,
}

/// Look up the seeded `kind='dabing'` playlist id (created by
/// `startup::ensure_dabing_playlist_exists`). `None` before the seed runs.
async fn dabing_playlist_id(pool: &sqlx::SqlitePool) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM playlists WHERE kind = 'dabing' ORDER BY id LIMIT 1")
        .fetch_optional(pool)
        .await
}

/// `GET /api/v1/dabing` — the Dabing playlist id + every dub-requested video,
/// newest request first.
pub async fn get_dabing(State(state): State<AppState>) -> impl IntoResponse {
    let playlist_id = match dabing_playlist_id(&state.pool).await {
        Ok(pid) => pid,
        Err(e) => {
            warn!("get_dabing playlist lookup error: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let videos = match models_dabing::list_dub_videos(&state.pool).await {
        Ok(v) => v,
        Err(e) => {
            warn!("get_dabing list error: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(DabingListResp {
        playlist_id,
        videos,
    })
    .into_response()
}

/// `POST /api/v1/dabing/import` — import the URL into the seeded Dabing playlist
/// (reusing the shared, cookie-gated import core), then flag it dub-requested so
/// the chain picks it up with priority. Returns 201 + the imported video.
pub async fn import_dabing(
    State(state): State<AppState>,
    Json(req): Json<DabingImportReq>,
) -> impl IntoResponse {
    let playlist_id = match dabing_playlist_id(&state.pool).await {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Dabing playlist not seeded yet".to_string(),
            )
                .into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let resp =
        match crate::api::routes_import::import_video_core(&state, &req.url, playlist_id).await {
            Ok(r) => r,
            Err((status, msg)) => return (status, msg).into_response(),
        };

    if let Err(e) = models_dabing::set_dub_requested(&state.pool, resp.video_id, true).await {
        warn!("import_dabing set_dub_requested error: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    (StatusCode::CREATED, Json(resp)).into_response()
}

/// `PATCH /api/v1/videos/{id}/dub` — toggle the per-video dub request. 204 on
/// success, 404 when no such video.
pub async fn patch_dub(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<DubToggleReq>,
) -> impl IntoResponse {
    match models_dabing::set_dub_requested(&state.pool, id, req.requested).await {
        Ok(0) => StatusCode::NOT_FOUND,
        Ok(_) => StatusCode::NO_CONTENT,
        Err(e) => {
            warn!("patch_dub error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// `PATCH /api/v1/videos/{id}/dub-mix` — set the per-video mixer blend ratio
/// (clamped 0.0..=1.0). #184 round A: the live `EngineCommand::SetDubMix` push is
/// awaited FIRST (so a playing dub re-blends in ~1.6 s), THEN the DB persist —
/// the reverse of the old order, where the persist's pool `acquire()` could park
/// for up to sqlx's 30 s default before the live gains were touched. A persist
/// failure is logged + returned as 500, but the live change already happened.
/// 200 + the stored value on success.
pub async fn patch_dub_mix(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<DubMixReq>,
) -> impl IntoResponse {
    // Same clamp the persist applies, computed up front so the live push carries
    // exactly what gets stored.
    let clamped = models_dabing::clamp_dub_ratio(req.ratio);
    let push = async {
        let _ = state
            .engine_tx
            .send(crate::EngineCommand::SetDubMix {
                video_id: id,
                ratio: clamped as f32,
            })
            .await;
    };
    let persist = models_dabing::set_dub_mix_ratio(&state.pool, id, req.ratio);
    match super::dabing_apply::apply_dub_mix(push, persist).await {
        Ok((_, 0)) => StatusCode::NOT_FOUND.into_response(),
        Ok((stored, _)) => {
            (StatusCode::OK, Json(serde_json::json!({ "ratio": stored }))).into_response()
        }
        Err(e) => {
            // The live change already applied; only the persist failed.
            warn!("patch_dub_mix persist error (live change applied): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
#[path = "dabing_tests.rs"]
mod tests;
