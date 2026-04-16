//! AI proxy management endpoints.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;

use crate::AppState;

#[cfg_attr(test, mutants::skip)]
pub async fn proxy_start(State(state): State<AppState>) -> impl IntoResponse {
    match state.ai_proxy.start().await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg_attr(test, mutants::skip)]
pub async fn proxy_stop(State(state): State<AppState>) -> impl IntoResponse {
    match state.ai_proxy.stop().await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg_attr(test, mutants::skip)]
pub async fn proxy_login(State(state): State<AppState>) -> impl IntoResponse {
    match state.ai_proxy.claude_login().await {
        Ok(url) => Json(serde_json::json!({"ok": true, "url": url})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg_attr(test, mutants::skip)]
pub async fn proxy_complete_login(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let callback_url = body["callback_url"].as_str().unwrap_or("");
    if callback_url.is_empty() {
        return Json(serde_json::json!({"ok": false, "error": "callback_url is required"}));
    }
    match state.ai_proxy.complete_login(callback_url).await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg_attr(test, mutants::skip)]
pub async fn ai_status(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.ai_proxy.status().await;
    Json(serde_json::json!(status))
}
