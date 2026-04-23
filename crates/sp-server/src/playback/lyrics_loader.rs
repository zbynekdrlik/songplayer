//! Helpers for loading per-song lyrics state from disk + DB. Extracted
//! from `playback/mod.rs` to keep that file under the 1000-line airuleset cap.
//!
//! Two entry points:
//! - [`load_lyrics_lead_ms`] — read the global `lyrics_lead_ms` setting,
//!   fall back to the default when absent or unparseable.
//! - [`load_lyrics_for_video`] — read the cached lyrics JSON for a video
//!   plus the per-song `videos.lyrics_time_offset_ms` shift.

use sqlx::{Row, SqlitePool};
use std::path::Path;
use tracing::warn;

/// Read the global `lyrics_lead_ms` setting from the DB, falling back to
/// [`crate::lyrics::renderer::DEFAULT_LYRICS_LEAD_MS`] when absent or
/// unparseable. An unparseable value warns but does not fail — lead time
/// is cosmetic, not correctness-critical.
#[cfg_attr(test, mutants::skip)] // I/O wrapper; the renderer's
// `lead_ms_is_applied_from_state` unit test exercises the constructor
// this wrapper feeds, which is the observable behavior.
pub(super) async fn load_lyrics_lead_ms(pool: &SqlitePool) -> u64 {
    use crate::lyrics::renderer::{DEFAULT_LYRICS_LEAD_MS, LYRICS_LEAD_SETTING_KEY};
    let val = match crate::db::models::get_setting(pool, LYRICS_LEAD_SETTING_KEY).await {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "lyrics_lead_ms: DB read failed, using default");
            return DEFAULT_LYRICS_LEAD_MS;
        }
    };
    let Some(raw) = val else {
        return DEFAULT_LYRICS_LEAD_MS;
    };
    match raw.trim().parse::<u64>() {
        Ok(n) => n,
        Err(e) => {
            warn!(
                raw = %raw,
                %e,
                "lyrics_lead_ms: unparseable, using default"
            );
            DEFAULT_LYRICS_LEAD_MS
        }
    }
}

/// Load lyrics JSON and per-song render-offset for a video from the cache
/// directory, if available. Returns `(track, offset_ms)` where `offset_ms`
/// comes from `videos.lyrics_time_offset_ms` (V16 migration) — defaults to
/// 0 when the column is NULL or the row is absent.
#[cfg_attr(test, mutants::skip)]
pub(super) async fn load_lyrics_for_video(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_id: i64,
) -> Result<Option<(sp_core::lyrics::LyricsTrack, i64)>, anyhow::Error> {
    let row = sqlx::query(
        "SELECT youtube_id, has_lyrics, lyrics_time_offset_ms FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };
    let has_lyrics: i64 = row.get("has_lyrics");
    if has_lyrics == 0 {
        return Ok(None);
    }
    let youtube_id: String = row.get("youtube_id");
    let offset_ms: i64 = row.try_get("lyrics_time_offset_ms").unwrap_or(0);
    let lyrics_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    if !lyrics_path.exists() {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&lyrics_path).await?;
    let track: sp_core::lyrics::LyricsTrack = serde_json::from_str(&content)?;
    Ok(Some((track, offset_ms)))
}
