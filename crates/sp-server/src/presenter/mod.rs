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

use crate::lyrics::renderer::PresenterLines;

/// #222: the lines last pushed (current + next, EN + SK) — the push dedup key.
pub type PushedLine = PresenterLines;

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

/// #222: the payload to push for `lines`, or `None` when its current line
/// was already pushed in BOTH languages (`last_seen` = its EN and SK). A
/// Slovak line that arrives later under the same English one is pushed.
/// Both languages always go out (Presenter's stage layout picks what to
/// show); a line with no translation sends "".
pub fn payload_for(
    last_seen: Option<&PushedLine>,
    lines: &PresenterLines,
    song: &str,
    artist: &str,
) -> Option<PresenterPayload> {
    let pushed = last_seen.is_some_and(|seen| {
        seen.current_en == lines.current_en && seen.current_sk == lines.current_sk
    });
    if pushed {
        return None;
    }
    let current_song = if artist.is_empty() {
        song.to_string()
    } else {
        format!("{song} - {artist}")
    };
    Some(PresenterPayload {
        // Wrap long lyric lines so they don't overflow the stage display;
        // many source lines are 40-60 chars and become unreadable on a
        // phone/tablet without breaks. Only the live-lyric fields are
        // wrapped — `currentSong` stays one line on purpose.
        current_text: payload::wrap_for_presenter(&lines.current_en),
        next_text: payload::wrap_for_presenter(&lines.next_en),
        current_song,
        next_song: String::new(),
        current_translation: payload::wrap_for_presenter(&lines.current_sk),
        next_translation: payload::wrap_for_presenter(&lines.next_sk),
    })
}

/// Line-change push helper used by the playback engine hot path. Spawns a
/// fire-and-forget `tokio::spawn(client.push(...))` when `payload_for` has a
/// payload (the current line changed in EN or SK), and returns the new
/// `last_seen` for the caller to persist. No-op when `client` is None (push
/// disabled).
#[cfg_attr(test, mutants::skip)] // spawn glue; `payload_for` is the tested decision
pub fn maybe_push_line(
    client: Option<&Arc<PresenterClient>>,
    last_seen: Option<PushedLine>,
    lines: PresenterLines,
    song: &str,
    artist: &str,
) -> Option<PushedLine> {
    let Some(client) = client else {
        return last_seen;
    };
    let Some(payload) = payload_for(last_seen.as_ref(), &lines, song, artist) else {
        return last_seen;
    };
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(e) = client.push(payload).await {
            tracing::warn!(?e, "presenter push failed (non-fatal)");
        }
    });
    Some(lines)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
