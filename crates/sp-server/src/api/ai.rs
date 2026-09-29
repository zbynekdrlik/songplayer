//! AI proxy management endpoints.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::AppState;

/// Body of `POST /api/v1/ai/proxy/complete-login`. #221: a typed body, never
/// `Json<serde_json::Value>` — `Value`'s own `Deserialize` re-parses the
/// string after a first key `$serde_json::private::RawValue` (the `raw_value`
/// feature is on in this build, `rust-workspace.md`). Here such a key is an
/// unknown field, skipped without being parsed.
#[derive(Debug, Deserialize)]
pub struct CompleteLoginRequest {
    /// The URL the Claude login redirected to; missing, `null` or empty =
    /// "callback_url is required".
    #[serde(default)]
    pub callback_url: Option<String>,
}

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
    Json(body): Json<CompleteLoginRequest>,
) -> impl IntoResponse {
    let callback_url = body.callback_url.as_deref().unwrap_or("");
    if callback_url.is_empty() {
        return Json(serde_json::json!({"ok": false, "error": "callback_url is required"}));
    }
    match state.ai_proxy.complete_login(callback_url).await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[cfg_attr(test, mutants::skip)]
/// The proxy's status plus `model`: the Claude model SongPlayer sends (the
/// `ai_model` setting, else `DEFAULT_AI_MODEL`, #145). The post-deploy AI step
/// makes a real completion with exactly this model.
pub async fn ai_status(State(state): State<AppState>) -> impl IntoResponse {
    let mut status = serde_json::json!(state.ai_proxy.status().await);
    status["model"] = serde_json::json!(state.ai_client.settings().model);
    Json(status)
}

#[cfg(test)]
#[path = "ai_tests.rs"]
mod tests;
