//! HTTP client helpers for the REST API.
//!
//! All paths are relative (e.g. `/api/v1/playlists`); the browser resolves
//! them against the current origin automatically.

use gloo_net::http::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// GET `path` and deserialise the JSON response.
pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("GET {} → {}", path, resp.status()));
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

/// POST JSON to `path` and deserialise the response.
pub async fn post_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::post(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// PUT JSON to `path` and deserialise the response.
pub async fn put_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::put(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PUT {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// PATCH JSON to `path` and deserialise the response.
#[allow(dead_code)]
pub async fn patch_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::patch(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PATCH {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// DELETE `path`.
pub async fn delete(path: &str) -> Result<(), String> {
    let resp = Request::delete(path)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("DELETE {} → {}", path, resp.status()));
    }
    Ok(())
}

/// POST `path` with no request body and discard the response body.
///
/// Used for playback control endpoints (`/api/v1/playback/{id}/{action}`)
/// that reply with `204 No Content`.
pub async fn post_empty(path: &str) -> Result<(), String> {
    let resp = Request::post(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    Ok(())
}

/// PUT JSON to `path` and discard the response body.
///
/// Used for playback mode updates and similar write endpoints that
/// reply with `204 No Content`.
pub async fn put_json_empty<T: Serialize>(path: &str, body: &T) -> Result<(), String> {
    let resp = Request::put(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PUT {} → {}", path, resp.status()));
    }
    Ok(())
}

// ── Lyrics API helpers ────────────────────────────────────────────────────────

/// GET the lyrics pipeline queue status.
pub async fn get_lyrics_queue() -> Result<serde_json::Value, String> {
    get("/api/v1/lyrics/queue").await
}

/// GET the list of songs with their lyrics state.
///
/// Pass `playlist_id` to filter to a single playlist.
pub async fn get_lyrics_songs(playlist_id: Option<i64>) -> Result<Vec<serde_json::Value>, String> {
    let url = if let Some(pid) = playlist_id {
        format!("/api/v1/lyrics/songs?playlist_id={pid}")
    } else {
        "/api/v1/lyrics/songs".into()
    };
    get(&url).await
}

/// GET detailed lyrics info for a single video.
pub async fn get_lyrics_song_detail(video_id: i64) -> Result<serde_json::Value, String> {
    get(&format!("/api/v1/lyrics/songs/{video_id}")).await
}

/// POST to reprocess specific videos by ID.
pub async fn post_reprocess_videos(video_ids: &[i64]) -> Result<serde_json::Value, String> {
    post_json(
        "/api/v1/lyrics/reprocess",
        &serde_json::json!({ "video_ids": video_ids }),
    )
    .await
}

/// POST to reprocess all videos in a playlist.
pub async fn post_reprocess_playlist(playlist_id: i64) -> Result<serde_json::Value, String> {
    post_json(
        "/api/v1/lyrics/reprocess",
        &serde_json::json!({ "playlist_id": playlist_id }),
    )
    .await
}

/// POST to reprocess all stale lyrics entries.
pub async fn post_reprocess_all_stale() -> Result<serde_json::Value, String> {
    post_json("/api/v1/lyrics/reprocess-all-stale", &serde_json::json!({})).await
}

/// POST to clear the manual (bucket 0) lyrics queue.
pub async fn post_clear_manual_queue() -> Result<serde_json::Value, String> {
    post_json("/api/v1/lyrics/clear-manual-queue", &serde_json::json!({})).await
}
