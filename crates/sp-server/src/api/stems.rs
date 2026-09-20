//! Karaoke stem operator actions (#177).
//!
//! `POST /api/v1/stems/{video_id}/enqueue` — "Zaradiť do fronty": reset a
//! video's stem bookkeeping so the oldest-first worker picks it on its next tick
//! (clears any failure backoff, re-opens a terminal `unsupported` row). It does
//! NOT jump the queue — the selector is oldest-first by id and
//! `stem_manual_priority` is #182 (out of scope) — so the button is labelled
//! "zaradiť do fronty", not "…teraz". Never gates playback (#162).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::warn;

use crate::AppState;

/// Re-enqueue a video for stem separation. Replies `{status, queue_position}`.
pub async fn enqueue(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    if let Err(e) = crate::db::models_stems::enqueue_stems(&state.pool, video_id).await {
        warn!(video_id, %e, "enqueue_stems failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    // #195: the tiered (in-use-first) position — matches the order the worker picks.
    let queue_position = crate::stems::queue_tiers::queue_position_now(
        Some(&state.ndi_health_registry),
        &state.pool,
        video_id,
    )
    .await
    .ok()
    .flatten();
    Json(serde_json::json!({
        "status": "enqueued",
        "queue_position": queue_position,
    }))
    .into_response()
}
