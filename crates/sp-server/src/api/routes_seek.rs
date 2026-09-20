//! #194 ROUND 1: the unified seek route. Lives in its own file so `api/routes.rs`
//! stays under the 1000-line cap.
//!
//! `POST /api/v1/playback/{playlist_id}/seek {position_ms}` — the single
//! operator seek control, on the same `/api/v1/playback/{id}/…` family as
//! play/pause/skip/previous/mode. Replaces the old, un-hardened
//! `POST /api/v1/playlists/{id}/seek` (which returned 204 unconditionally). The
//! same downstream path as before: `EngineCommand::Seek` → `PlaybackEngine::seek`
//! → `PipelineCommand::Seek` (the decode loop clears the audio emitter ring on
//! seek; lyrics/subtitle position follows the pipeline position events).
//!
//! Hardening:
//! - `404 Not Found` when the playlist row does not exist.
//! - `409 Conflict` when nothing is playing on that playlist (the pipeline's own
//!   Seek handler is a no-op then, so a silent 204 would mislead the operator).
//! - `409 Conflict` when the playing song's duration is unknown (NULL/0/errored
//!   lookup): there is no upper clamp bound, so forwarding the client position
//!   would scrub UNCLAMPED — refuse rather than seek unbounded (#198 item 7).
//! - `position_ms` is clamped to `0..=duration` of the playing song via the pure
//!   `sp_core::seek_model::seek_target_ms` helper (delta = 0), so a stale UI can
//!   never scrub past the end.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sqlx::Row;

use crate::{AppState, EngineCommand};

/// Body of `POST /api/v1/playback/{playlist_id}/seek`.
#[derive(Debug, Deserialize)]
pub struct SeekReq {
    pub position_ms: u64,
}

/// POST /api/v1/playback/{playlist_id}/seek — jump the currently-playing song
/// on `playlist_id` to `position_ms` (clamped to the song's duration).
pub async fn post_seek(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<SeekReq>,
) -> impl IntoResponse {
    // 404: the playlist must exist.
    match sqlx::query("SELECT id FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(playlist_id, %e, "post_seek: playlist lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    // 409: something must be playing on this playlist (the now-playing registry
    // holds the playing video id, set on Started / cleared on Stop). A paused
    // song keeps its entry, so seeking a paused song is allowed.
    let Some(video_id) = crate::now_playing::global()
        .snapshot()
        .into_iter()
        .find(|(p, _)| *p == playlist_id)
        .map(|(_, v)| v)
    else {
        return StatusCode::CONFLICT.into_response();
    };

    // Clamp to the playing song's duration (defence in depth — the UI already
    // caps the slider). A known duration is `Some(d>0)`; a NULL/0/errored lookup
    // is unknown.
    let duration_ms: Option<u64> = match sqlx::query("SELECT duration_ms FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(Some(row)) => row
            .try_get::<Option<i64>, _>("duration_ms")
            .ok()
            .flatten()
            .filter(|d| *d > 0)
            .map(|d| d as u64),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(video_id, %e, "post_seek: duration lookup failed");
            None
        }
    };

    // #198 item 7: without a known duration there is no upper clamp bound, so an
    // arbitrary client position would reach EngineCommand::Seek UNCLAMPED. Refuse
    // with 409 rather than forward an unbounded seek — a playing, seekable song
    // has a known duration; an un-probed one is not safely seekable.
    let Some(duration_ms) = duration_ms else {
        return StatusCode::CONFLICT.into_response();
    };
    let position_ms = sp_core::seek_model::seek_target_ms(req.position_ms, 0, duration_ms);

    match state
        .engine_tx
        .send(EngineCommand::Seek {
            playlist_id,
            position_ms,
        })
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
#[path = "routes_seek_tests.rs"]
mod tests;
