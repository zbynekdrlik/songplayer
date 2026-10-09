//! #242: a playlist's own sound over HTTP.
//!
//! - `GET /api/v1/playlists/{id}/audio` → `{gain_db, eq, generation}`: the
//!   row's settings and the live register's generation (it moves on every
//!   set, so a client sees the change applied).
//! - `PUT /api/v1/playlists/{id}/audio` with `{gain_db, eq}`
//!   (`sp_core::audio_fx::PlaylistFx`, unknown fields refused) → 204. A body
//!   that does not read, or a value past a limit, is 400 with the reason and
//!   nothing written; an unknown playlist is 404. The row is written FIRST,
//!   then the live register, so a restart never loses a change the stream
//!   already played.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sp_core::audio_fx::{EqBand, PlaylistFx, validate};
use tracing::{error, info};

use super::AppState;
use crate::db::models_playlist_fx::{get_playlist_fx, set_playlist_fx};

/// The answer of `GET …/audio`.
#[derive(Debug, Serialize)]
pub struct PlaylistAudioView {
    pub gain_db: f64,
    pub eq: Vec<EqBand>,
    pub generation: u64,
}

pub async fn get_playlist_audio(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match get_playlist_fx(&state.pool, id).await {
        Ok(Some(fx)) => Json(PlaylistAudioView {
            gain_db: fx.gain_db,
            eq: fx.eq,
            generation: crate::playback::playlist_fx::global().slot(id).generation(),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!(id, %e, "playlist audio: reading the row failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn put_playlist_audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let fx: PlaylistFx = match serde_json::from_slice(&body) {
        Ok(fx) => fx,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("audio body: {e}")).into_response(),
    };
    if let Err(e) = validate(&fx) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    match set_playlist_fx(&state.pool, id, &fx).await {
        Ok(true) => {
            info!(
                id,
                gain_db = fx.gain_db,
                bands = fx.eq.len(),
                enabled = fx.eq.iter().filter(|b| b.enabled).count(),
                "playlist audio set"
            );
            crate::playback::playlist_fx::global().set(id, fx);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!(id, %e, "playlist audio: the row was not written");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
#[path = "playlist_audio_tests.rs"]
mod tests;
