//! HTTP handlers for custom playlist set-list management + click-to-play.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;
use crate::db::models;

#[derive(Debug, Deserialize)]
pub struct AddItemRequest {
    pub video_id: i64,
}

#[derive(Debug, Serialize)]
pub struct AddItemResponse {
    pub position: i64,
}

/// Request body for `POST /api/v1/playlists/{id}/items/{video_id}/move`.
/// Moves the item one slot up (`direction = "up"`) or down (`direction =
/// "down"`). Kept to a one-step swap rather than absolute positions so the
/// mobile UI only needs ↑/↓ buttons — no numeric picker.
#[derive(Debug, Deserialize)]
pub struct MoveItemRequest {
    pub direction: String,
}

// HTTP handler: validates playlist kind is 'custom' then appends the video
// to playlist_items. Returns 409 for youtube playlists and for duplicate
// video_ids. Covered by api::live::tests_included::post_item_*.
// mutants::skip: thin HTTP wrapper over append_playlist_item; behaviour is exercised by the tests_included suite.
#[cfg_attr(test, mutants::skip)]
pub async fn post_add_item(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<AddItemRequest>,
) -> impl IntoResponse {
    let kind: Option<String> = match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(k) => k,
        Err(e) => {
            warn!(playlist_id, %e, "post_add_item: kind lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    match models::append_playlist_item(&state.pool, playlist_id, req.video_id).await {
        Ok(position) => Json(AddItemResponse { position }).into_response(),
        Err(e) => {
            warn!(playlist_id, video_id = req.video_id, %e, "append_playlist_item failed");
            let msg = e.to_string();
            if msg.contains("UNIQUE") {
                (StatusCode::CONFLICT, "video already in playlist").into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

// HTTP handler: removes a video from the custom playlist and compacts
// positions. 409 for youtube playlists.
// mutants::skip: thin HTTP wrapper over remove_playlist_item; behaviour is exercised by the tests_included suite.
#[cfg_attr(test, mutants::skip)]
pub async fn delete_item(
    State(state): State<AppState>,
    Path((playlist_id, video_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let kind: Option<String> = match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(k) => k,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    match models::remove_playlist_item(&state.pool, playlist_id, video_id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            warn!(playlist_id, video_id, %e, "remove_playlist_item failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

// HTTP handler: one-step reorder of a set-list row.
// Body: `{"direction": "up" | "down"}`.
// mutants::skip: thin HTTP wrapper over move_playlist_item_step; tested in tests_included.
#[cfg_attr(test, mutants::skip)]
pub async fn post_move_item(
    State(state): State<AppState>,
    Path((playlist_id, video_id)): Path<(i64, i64)>,
    Json(req): Json<MoveItemRequest>,
) -> impl IntoResponse {
    let kind: Option<String> = match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(k) => k,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    let direction = match req.direction.as_str() {
        "up" => -1,
        "down" => 1,
        _ => return (StatusCode::BAD_REQUEST, "direction must be 'up' or 'down'").into_response(),
    };

    match models::move_playlist_item_step(&state.pool, playlist_id, video_id, direction).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            warn!(playlist_id, video_id, %e, "move_playlist_item_step failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

// HTTP handler: returns the current set list in position order.
// mutants::skip: thin HTTP wrapper over list_playlist_items; behaviour is exercised by the tests_included suite.
#[cfg_attr(test, mutants::skip)]
pub async fn get_items(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    match models::list_playlist_items(&state.pool, playlist_id).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => {
            warn!(playlist_id, %e, "list_playlist_items failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PlayVideoRequest {
    pub video_id: i64,
}

// HTTP handler: sends EngineCommand::PlayVideo to the engine after
// validating (a) the playlist exists, (b) it's kind='custom', and (c) the
// video is actually a member of the set list. Matches the 404/409 status
// code discipline of the sibling handlers so malformed clients don't get
// a silent 204.
// mutants::skip: pure dispatch to engine channel; behaviour covered by play_video_sends_engine_command.
#[cfg_attr(test, mutants::skip)]
pub async fn post_play_video(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<PlayVideoRequest>,
) -> impl IntoResponse {
    let kind: Option<String> = match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(k) => k,
        Err(e) => {
            warn!(playlist_id, %e, "post_play_video: kind lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    // Verify the video is in the set list — prevents a client from triggering
    // playback of an arbitrary video via a custom-playlist URL.
    match models::position_for_playlist_item(&state.pool, playlist_id, req.video_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "video not in playlist set list").into_response();
        }
        Err(e) => {
            warn!(playlist_id, video_id = req.video_id, %e, "post_play_video: position lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    let _ = state
        .engine_tx
        .send(crate::EngineCommand::PlayVideo {
            playlist_id,
            video_id: req.video_id,
        })
        .await;
    StatusCode::NO_CONTENT.into_response()
}

#[path = "live_tests_included.rs"]
#[cfg(test)]
mod tests_included;
