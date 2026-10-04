//! #225 unit 2: a playlist's playback mode has ONE persisted truth, its
//! `playlists.playback_mode` row. Lives in its own file so `api/routes.rs`
//! stays under the 1000-line cap.
//!
//! Every mode change writes the row FIRST and only then tells the engine
//! (`EngineCommand::SetMode` → `PlaybackEngine::apply_mode`, which tells the
//! dashboards). Every pipeline starts in its row's mode
//! (`db::models_playlists::row_mode`), so a restart plays what the row says.
//! The three ways in:
//!
//! - `PUT /api/v1/playback/{id}/mode` ([`set_mode`]), the dashboard's mode
//!   select;
//! - the WS `ClientMsg::SetMode` (`api/websocket.rs`), through the same
//!   [`persist_then_tell`];
//! - the playlist PATCH (`routes::update_playlist`), which writes the row
//!   with its other fields, then calls [`tell_engine`].
//!
//! A write that fails is logged and answered (500, or a WS `Error` to every
//! open dashboard), and the engine is never told, so it keeps the mode the
//! row still holds. An unknown requested mode is refused (400) by the PUT,
//! the PATCH and the playlist POST ([`requested_mode`]), so every row the
//! API writes parses.
//!
//! [`MODE_ORDER`] is held across "write the row, tell the engine" (and by
//! the PATCH and the playlist DELETE), so the engine is told concurrent
//! changes in the order the row took them. Without it, the row's last write
//! and the engine's last mode could differ, or a mode could reach the engine
//! after it forgot a deleted playlist.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

use crate::{AppState, EngineCommand};

/// Body of `PUT /api/v1/playback/{playlist_id}/mode`.
#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    pub mode: String,
}

/// Held across "write the row, then tell the engine" (module doc).
pub(crate) static MODE_ORDER: Mutex<()> = Mutex::const_new(());

/// A requested mode (the PUT's body, the PATCH's or the POST's field) by
/// its canonical name: `Ok(None)` when none is given, `Err(400)` for an
/// unknown one, WARNed with the playlist (none yet for the POST) and at most
/// 32 of its characters, escaped.
pub(crate) fn requested_mode(
    playlist_id: Option<i64>,
    requested: Option<&str>,
) -> Result<Option<PlaybackMode>, StatusCode> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    match PlaybackMode::parse(requested) {
        Some(mode) => Ok(Some(mode)),
        None => {
            let shown: String = requested.chars().take(32).collect();
            warn!(?playlist_id, requested = ?shown, "unknown playback mode refused");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// `PUT /api/v1/playback/{playlist_id}/mode` — the dashboard's mode select.
/// 204 once the row holds the mode and the engine was told; 400 for an
/// unknown mode; 404 for an unknown playlist; 500 when the row could not be
/// written (the engine keeps its mode).
pub async fn set_mode(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(body): Json<SetModeRequest>,
) -> StatusCode {
    let Ok(Some(mode)) = requested_mode(Some(playlist_id), Some(&body.mode)) else {
        return StatusCode::BAD_REQUEST;
    };
    match persist_then_tell(&state.pool, &state.engine_tx, playlist_id, mode).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Write `mode` into the playlist's row, THEN tell the engine. `Ok(false)`:
/// no such playlist, nothing written and the engine not told. `Err`: the
/// row was not written (logged here) and the engine not told, so it keeps
/// its mode.
pub(crate) async fn persist_then_tell(
    pool: &SqlitePool,
    engine_tx: &mpsc::Sender<EngineCommand>,
    playlist_id: i64,
    mode: PlaybackMode,
) -> Result<bool, sqlx::Error> {
    let _order = MODE_ORDER.lock().await;
    let written = sqlx::query(
        "UPDATE playlists SET playback_mode = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(mode.as_str())
    .bind(playlist_id)
    .execute(pool)
    .await
    .inspect_err(|e| {
        error!(
            playlist_id,
            mode = mode.as_str(),
            %e,
            "playback mode not saved — the engine keeps its mode"
        );
    })?;
    if written.rows_affected() == 0 {
        warn!(
            playlist_id,
            "playback mode for an unknown playlist — nothing saved"
        );
        return Ok(false);
    }
    tell_engine(engine_tx, playlist_id, mode).await;
    Ok(true)
}

/// Tell the engine a mode its playlist's row now holds. The caller wrote the
/// row first, holding [`MODE_ORDER`].
pub(crate) async fn tell_engine(
    engine_tx: &mpsc::Sender<EngineCommand>,
    playlist_id: i64,
    mode: PlaybackMode,
) {
    info!(
        playlist_id,
        mode = mode.as_str(),
        "playback mode saved — telling the engine"
    );
    let _ = engine_tx
        .send(EngineCommand::SetMode { playlist_id, mode })
        .await;
}
