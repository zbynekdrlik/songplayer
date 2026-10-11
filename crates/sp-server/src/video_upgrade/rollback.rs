//! #223 S12b: an upgraded song that fails to open gets its old video back
//! (design comment 6103547599).
//!
//! The engine's open failure (`failure_retry::video_failed`) hands the
//! failed row to [`after_failed_open`]: when the row's video was upgraded
//! and its `.prev` is still there, the `.prev` goes back under the name (one
//! replacing rename under the song-file lock), the song is `rolled_back` and
//! settled at the live cap (the worker never upgrades it again at that
//! cap), and its V34 format is unknown again (NULL). The engine's own retry
//! then opens the old video. Any other failure changes nothing here.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::retention::prev_of;
use crate::downloader::cache::SONG_FILES;

/// What [`after_failed_open`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RolledBack {
    /// The old video is back under the name.
    Restored,
    /// Not an upgraded song with a `.prev`: nothing to do.
    NotUpgraded,
    /// The rename back failed: nothing changed (logged).
    Failed(String),
}

/// The failed row's video when it was upgraded: its YouTube id and path.
async fn upgraded_video(
    pool: &SqlitePool,
    video_row_id: i64,
) -> Result<Option<(String, PathBuf)>, sqlx::Error> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT youtube_id, file_path FROM videos \
         WHERE id = ? AND video_upgrade_state = 'upgraded' AND file_path IS NOT NULL",
    )
    .bind(video_row_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(youtube_id, path)| (youtube_id, PathBuf::from(path))))
}

/// Put the old video back after a failed open of row `video_row_id`
/// (module doc), recording it at `now_ms`.
pub(crate) async fn after_failed_open(
    pool: &SqlitePool,
    video_row_id: i64,
    now_ms: i64,
) -> RolledBack {
    let _files = SONG_FILES.lock().await;
    let (youtube_id, video) = match upgraded_video(pool, video_row_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return RolledBack::NotUpgraded,
        Err(e) => {
            warn!(
                video_row_id,
                "video upgrade: the failed row could not be read: {e}"
            );
            return RolledBack::Failed(e.to_string());
        }
    };
    let Some(prev) = prev_of(&video) else {
        return RolledBack::NotUpgraded;
    };
    if let Err(e) = restore(&prev, &video) {
        warn!(youtube_id = %youtube_id, "video upgrade: the old video could not be put back: {e}");
        return RolledBack::Failed(e);
    }
    let cap = crate::downloader::format::live_cap(pool).await;
    if let Err(e) = super::record(pool, &youtube_id, Some(cap), "rolled_back", now_ms).await {
        warn!(youtube_id = %youtube_id, "video upgrade: the rollback was not recorded: {e}");
    }
    if let Err(e) = crate::downloader::format::record(pool, video_row_id, None).await {
        warn!(youtube_id = %youtube_id, "video upgrade: the old format was not recorded: {e}");
    }
    info!(
        youtube_id = %youtube_id,
        cap,
        "video upgrade: rolled back after a failed open - the old video is back"
    );
    RolledBack::Restored
}

/// `prev` renamed back over `video` (one replacing rename).
pub(crate) fn restore(prev: &Path, video: &Path) -> Result<(), String> {
    std::fs::rename(prev, video).map_err(|e| format!("the rename of {} back: {e}", prev.display()))
}

#[cfg(test)]
#[path = "rollback_tests.rs"]
mod tests;
