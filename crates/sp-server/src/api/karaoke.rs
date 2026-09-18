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
///
/// #177: also returns `now_playing[]` — one entry per currently-playing
/// pipeline: `{playlist_id, video_id, title, stems_state, stems_error,
/// queue_position}` — so the panel can bind to the SELECTED playlist's song and
/// show whether ITS stems are ready. The set comes from the process-global
/// now-playing registry; per-song stems state is read from the same DB the stem
/// worker writes (never re-derived).
pub async fn get_karaoke(State(state): State<AppState>) -> impl IntoResponse {
    let control = crate::stems::control::global();
    let (pending, done) = crate::db::models_stems::count_stems_progress(&state.pool)
        .await
        .unwrap_or((0, 0));

    let in_flight = crate::stems::progress::in_flight();
    let mut now_playing = Vec::new();
    for (playlist_id, video_id) in crate::now_playing::global().snapshot() {
        let info = match crate::db::models_stems::video_stems_info(
            &state.pool,
            video_id,
            in_flight == Some(video_id),
        )
        .await
        {
            Ok(Some(info)) => info,
            _ => continue, // row vanished; skip rather than emit a half entry
        };
        let queue_position = crate::db::models_stems::queue_position(&state.pool, video_id)
            .await
            .ok()
            .flatten();
        now_playing.push(serde_json::json!({
            "playlist_id": playlist_id,
            "video_id": video_id,
            "title": info.title,
            "stems_state": info.state.as_str(),
            "stems_error": stems_error(info.state, info.attempts),
            "queue_position": queue_position,
        }));
    }

    Json(serde_json::json!({
        "mode": control.mode().as_str(),
        "vocal_gain": control.vocal_gain(),
        "stems_pending": pending,
        "stems_done": done,
        "now_playing": now_playing,
    }))
}

/// A human reason string for the non-ready states the panel surfaces (#177).
/// There is no per-song stem error text stored in the DB, so this is derived
/// from the state + attempt count; `None` for states that need no explanation.
fn stems_error(state: crate::db::models_stems::StemsState, attempts: i64) -> Option<String> {
    use crate::db::models_stems::StemsState;
    match state {
        StemsState::Failed => Some(format!(
            "posledný pokus o spracovanie stemov zlyhal (pokusov: {attempts})"
        )),
        StemsState::Unavailable => {
            Some("skladba je pridlhá alebo bez vokálov — stemy nie sú dostupné".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "karaoke_tests.rs"]
mod tests;

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
