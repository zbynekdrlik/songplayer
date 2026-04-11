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
