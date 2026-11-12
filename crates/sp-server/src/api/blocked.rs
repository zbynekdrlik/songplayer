//! #265: the node's song block list over HTTP (the `/blok` page's routes).
//!
//! - `GET /api/v1/blocked` → 200, every blocked song (`BlockedSong`), the
//!   newest block first.
//! - `POST /api/v1/blocked {youtube_id}` (typed `BlockRequest`, never
//!   `serde_json::Value`) → 201 with the stored row; a song this node does
//!   not have → 404; a second block keeps the first time (201 again).
//! - `DELETE /api/v1/blocked/{youtube_id}` → 204; not blocked → 404.
//!
//! One INFO per block and unblock, with the title. A block acts at the next
//! automatic pick (`db::pickable_row!`): a song playing now plays to its end.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sp_core::song_block::BlockRequest;
use tracing::{debug, info, warn};

use crate::AppState;
use crate::db::models_blocked;

fn db_error(what: &str, e: sqlx::Error) -> Response {
    warn!(%e, "song block: {what} failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("the block list could not be read or written: {e}"),
    )
        .into_response()
}

/// `GET /api/v1/blocked`.
pub async fn list_blocked(State(state): State<AppState>) -> Response {
    match models_blocked::list(&state.pool).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => db_error("the list", e),
    }
}

/// `POST /api/v1/blocked`.
pub async fn block_song(State(state): State<AppState>, Json(req): Json<BlockRequest>) -> Response {
    let youtube_id = req.youtube_id.trim();
    let title = match models_blocked::song_title(&state.pool, youtube_id).await {
        Ok(Some(title)) => title,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "no such song on this node").into_response();
        }
        Err(e) => return db_error("the song's lookup", e),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    match models_blocked::block(&state.pool, youtube_id, &title, now_ms).await {
        Ok((stored, added)) => {
            if added {
                info!(youtube_id, title = %stored.title, "song block: a song is blocked on this node - never picked again");
            } else {
                debug!(
                    youtube_id,
                    "song block: already blocked - its first block is kept"
                );
            }
            (StatusCode::CREATED, Json(stored)).into_response()
        }
        Err(e) => db_error("the block", e),
    }
}

/// `DELETE /api/v1/blocked/{youtube_id}`.
pub async fn unblock_song(
    State(state): State<AppState>,
    Path(youtube_id): Path<String>,
) -> Response {
    match models_blocked::unblock(&state.pool, &youtube_id).await {
        Ok(Some(title)) => {
            info!(
                youtube_id,
                title, "song block: a song is unblocked on this node"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "the song is not blocked").into_response(),
        Err(e) => db_error("the unblock", e),
    }
}

#[cfg(test)]
#[path = "blocked_tests.rs"]
mod tests;
