//! #229: the settings API — `GET /api/v1/settings` (every stored setting) and
//! `PATCH /api/v1/settings` (a flat map of settings to write).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sqlx::Row;
use tracing::warn;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    #[serde(flatten)]
    pub settings: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Settings endpoints
// ---------------------------------------------------------------------------

pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
        .fetch_all(&state.pool)
        .await;

    match rows {
        Ok(rows) => {
            let mut map = serde_json::Map::new();
            for r in &rows {
                let key: String = r.get("key");
                let value: String = r.get("value");
                map.insert(key, serde_json::Value::String(value));
            }
            Json(serde_json::Value::Object(map)).into_response()
        }
        Err(e) => {
            warn!("get_settings error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn update_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    for (key, value) in &body.settings {
        if let Err(e) = crate::db::models::set_setting(&state.pool, key, value).await {
            warn!("update_settings error for key {key}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
