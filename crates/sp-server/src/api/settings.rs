//! #229: the settings API — `GET /api/v1/settings` (every stored setting) and
//! `PATCH /api/v1/settings` (a flat map of settings to write).
//!
//! - GET masks every secret setting: one on `sp_core::config::SECRET_SETTINGS`
//!   or named like one (`_key`, `_token`, `_password`, `_secret`, so a retired
//!   credential still in a node's database is masked too,
//!   `is_secret_setting`) reads as `SECRET_MASK`. `peers` shows each peer's
//!   key and Cloudflare secret as the mask (`peer::config::shown_peers`). An
//!   empty or blank value reads as stored: it reveals nothing, and the form
//!   shows an empty field.
//! - A PATCH value exactly `SECRET_MASK` for a masked setting keeps the stored
//!   value: nothing is written for it, and the exchange checks do not count
//!   it as sent. Any other value replaces the stored one; `""` clears it. A
//!   masked peer sent back inside `peers` takes the stored peer's secrets,
//!   but only while its `base_url` (for the Cloudflare secret also its
//!   `cf_client_id`) is the stored one (`peer::config::unmask_peers`).
//! - An exchange setting that does not hold (`node_name`, `peer_api_key`,
//!   `peers`: `peer::config::checked`) refuses the whole PATCH with 400 and
//!   the reason, before anything is written. The reason names keys, peers and
//!   positions, never a secret.
//!
//! The workers read the settings from the database, never through this API,
//! so they always see the secrets in clear.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sp_core::config::{SECRET_MASK, SETTING_PEERS, is_secret_setting};
use sqlx::{Row, SqlitePool};
use tracing::warn;

use crate::AppState;

/// A PATCH body. No `Debug`: the map carries secrets in clear.
#[derive(Deserialize)]
pub struct UpdateSettingsRequest {
    #[serde(flatten)]
    pub settings: HashMap<String, String>,
}

/// Whether GET masks `key`: a secret setting whole, `peers` the secrets in it.
fn masked(key: &str) -> bool {
    key == SETTING_PEERS || is_secret_setting(key)
}

/// `value` of `key` as GET shows it: an empty (or blank) value and a setting
/// that is not masked as stored, `peers` with each peer's secrets masked, any
/// other masked setting as [`SECRET_MASK`].
pub fn shown(key: &str, value: String) -> String {
    if value.trim().is_empty() || !masked(key) {
        value
    } else if key == SETTING_PEERS {
        crate::peer::config::shown_peers(&value)
    } else {
        SECRET_MASK.to_string()
    }
}

/// A PATCH value that keeps the stored one: exactly the mask, for a masked
/// setting.
fn keeps_stored(key: &str, value: &str) -> bool {
    value == SECRET_MASK && masked(key)
}

/// The `(key, value)` writes of a PATCH, in key order, or why it is refused
/// (then nothing may be written). A kept setting (`keeps_stored`) is left
/// out, and the exchange checks see only the settings really sent: a `peers`
/// kept on the mask must not spare a `node_name` the check against the
/// stored peers.
pub async fn prepare(
    pool: &SqlitePool,
    incoming: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let sent: HashMap<String, String> = incoming
        .iter()
        .filter(|(key, value)| !keeps_stored(key, value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut keys: Vec<&String> = sent.keys().collect();
    keys.sort();
    let mut writes = Vec::new();
    for key in keys {
        let value = crate::peer::config::checked(pool, key, &sent[key], &sent).await?;
        writes.push((key.clone(), value));
    }
    Ok(writes)
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
                let value = shown(&key, value);
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
    let writes = match prepare(&state.pool, &body.settings).await {
        Ok(writes) => writes,
        Err(reason) => {
            warn!("settings PATCH refused: {reason}");
            return (StatusCode::BAD_REQUEST, reason).into_response();
        }
    };
    for (key, value) in &writes {
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
