//! Karaoke live-control endpoints (#14).
//!
//! `GET /api/v1/karaoke`  → current mode + vocal gain + stem-generation progress.
//! `POST /api/v1/karaoke` → set mode + vocal gain (routed to the engine, which
//! updates the live control, persists, broadcasts, and reloads playing pipelines
//! on a mode change).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sp_core::playback::KaraokeMode;

use crate::{AppState, EngineCommand};

/// Body for `POST /api/v1/karaoke`. `mode` is one of `full_mix` / `karaoke_low`
/// / `vocals_only` / `instrumental_only` (unknown → FullMix). `vocal_gain`
/// (0.0..=1.0) only affects KaraokeLow; omitted → keep the current gain.
#[derive(Debug, Deserialize)]
pub struct SetKaraokeRequest {
    pub mode: String,
    #[serde(default)]
    pub vocal_gain: Option<f32>,
}

/// GET the live karaoke state + stem progress for the dashboard.
pub async fn get_karaoke(State(state): State<AppState>) -> impl IntoResponse {
    let control = crate::stems::control::global();
    let (pending, done) = crate::db::models::count_stems_progress(&state.pool)
        .await
        .unwrap_or((0, 0));
    Json(serde_json::json!({
        "mode": control.mode().as_str(),
        "vocal_gain": control.vocal_gain(),
        "stems_pending": pending,
        "stems_done": done,
    }))
}

/// POST a new karaoke mode + vocal gain. Replies 204 No Content.
pub async fn set_karaoke(
    State(state): State<AppState>,
    Json(body): Json<SetKaraokeRequest>,
) -> impl IntoResponse {
    let mode = KaraokeMode::from_str_lossy(&body.mode);
    let vocal_gain = body
        .vocal_gain
        .unwrap_or_else(|| crate::stems::control::global().vocal_gain());
    let _ = state
        .engine_tx
        .send(EngineCommand::SetKaraoke { mode, vocal_gain })
        .await;
    StatusCode::NO_CONTENT
}
