//! Videos list endpoint (#177).
//!
//! Split out of `routes.rs` (1000-line cap) so the videos payload can carry the
//! additive per-song `stems_state` marker the dashboard's song list renders. The
//! base rows come from `models::get_videos_for_playlist`; the stems state is
//! merged from `models_stems::stems_state_map` (read from the same DB the stem
//! worker writes — never re-derived).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::warn;

use crate::AppState;

/// `GET /api/v1/playlists/{id}/videos` — the playlist's videos, each row
/// carrying its `stems_state` (`ready` / `queued` / `processing` / `unavailable`
/// / `failed`) so the operator can see which songs karaoke works on.
pub async fn list_videos(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let mut videos = match crate::db::models::get_videos_for_playlist(&state.pool, id).await {
        Ok(v) => v,
        Err(e) => {
            warn!("list_videos error: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let in_flight = crate::stems::progress::in_flight();
    match crate::db::models_stems::stems_state_map(&state.pool, id, in_flight).await {
        Ok(map) => {
            for v in videos.iter_mut() {
                v.stems_state = map.get(&v.id).cloned();
            }
        }
        Err(e) => warn!("list_videos stems_state_map error: {e}"),
    }
    Json(videos).into_response()
}
