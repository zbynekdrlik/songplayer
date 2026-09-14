//! Live video preview HTTP handler (#15 part 2). Extracted from `routes.rs`
//! to keep it under the 1000-line cap. The bytes are sampled opportunistically
//! from already-decoded frames by `playback::preview::PreviewTap` and never
//! touch the NDI submit / genlock / pacing path.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::AppState;

/// GET /api/v1/playback/{playlist_id}/preview.jpg — the live low-res video
/// preview of the currently-playing song for one playlist.
///
/// Returns `200 image/jpeg` with the latest sampled frame, `204 No Content`
/// when the pipeline is idle / has not produced a frame yet, or `404` when no
/// pipeline exists for that playlist. Each request marks a viewer (a TTL), so
/// the pipeline only spends any effort sampling while the dashboard is polling.
pub async fn get_playback_preview(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    match state.preview_registry.get(playlist_id) {
        Some(tap) => {
            tap.note_viewer_request();
            match tap.latest_jpeg() {
                Some(jpeg) => (
                    StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                        (axum::http::header::CACHE_CONTROL, "no-store"),
                    ],
                    jpeg,
                )
                    .into_response(),
                None => StatusCode::NO_CONTENT.into_response(),
            }
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
#[path = "routes_tests_preview.rs"]
mod tests;
