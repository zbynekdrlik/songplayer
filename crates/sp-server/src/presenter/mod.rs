//! HTTP push to the Presenter stage-display API. Used by the playback
//! engine's line-change hook (T2.4) to inform band singers what line is
//! sung and what comes next on their stage displays, independently of
//! whatever the audience wall shows.
//!
//! Prod host: http://10.77.9.205/api/stage
//! Dev host:  http://10.77.8.134:8080/api/stage

pub mod client;
pub mod payload;

pub use client::{PresenterClient, PresenterError};
pub use payload::PresenterPayload;

use std::sync::Arc;

/// Default Presenter API endpoint when `presenter_url` setting is empty.
pub const DEFAULT_URL: &str = "http://10.77.9.205/api/stage";

/// Build a `PresenterClient` from the two `presenter_*` DB settings, or
/// return None when disabled. Called from lib.rs startup once per process.
#[cfg_attr(test, mutants::skip)]
pub async fn build_from_settings(
    pool: &sqlx::SqlitePool,
) -> Result<Option<Arc<PresenterClient>>, sqlx::Error> {
    let url = crate::db::models::get_setting(pool, "presenter_url")
        .await?
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let enabled = crate::db::models::get_setting(pool, "presenter_enabled")
        .await?
        .map(|s| {
            !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "off" | "no"
            )
        })
        .unwrap_or(true);
    if enabled {
        tracing::info!(%url, "presenter: push enabled");
        Ok(Some(Arc::new(PresenterClient::new(url))))
    } else {
        tracing::info!("presenter: push DISABLED via settings");
        Ok(None)
    }
}

/// Line-change push helper used by the playback engine hot path. Spawns a
/// fire-and-forget `tokio::spawn(client.push(...))` when `current_en`
/// differs from `last_seen`, and returns the new `last_seen` for the caller
/// to persist. No-op when `client` is None (push disabled).
#[cfg_attr(test, mutants::skip)]
pub fn maybe_push_line(
    client: Option<&Arc<PresenterClient>>,
    last_seen: Option<String>,
    current_en: String,
    next_en: String,
    song: &str,
    artist: &str,
) -> Option<String> {
    let Some(client) = client else {
        return last_seen;
    };
    if last_seen.as_deref() == Some(current_en.as_str()) {
        return last_seen;
    }
    let current_song = if artist.is_empty() {
        song.to_string()
    } else {
        format!("{song} - {artist}")
    };
    let payload = PresenterPayload {
        current_text: current_en.clone(),
        next_text: next_en,
        current_song,
        next_song: String::new(),
    };
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(e) = client.push(payload).await {
            tracing::warn!(?e, "presenter push failed (non-fatal)");
        }
    });
    Some(current_en)
}
