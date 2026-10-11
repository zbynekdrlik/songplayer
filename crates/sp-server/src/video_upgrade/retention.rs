//! #223 S12b: how long an upgraded song keeps its old video as `<name>.prev`
//! (design comment 6103547599).
//!
//! The `.prev` is the rollback's source ([`super::rollback`]), so it stays
//! until the song is known to play: it is deleted once the song has
//! `Started` after its upgrade (`play_history` is recorded at `Started`), or
//! [`PREV_KEEP_MS`] after the upgrade, and the oldest go first while all of
//! them hold more than [`PREV_BUDGET_BYTES`]. A `.prev` no row's video
//! names any more (a rename since) is deleted at once.
//!
//! The worker sweeps every tick, switch on or off ([`sweep`]); the cache
//! scan at startup leaves `.prev` files alone (no pattern of it matches).

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::swap::prev_path;
use crate::downloader::cache::SONG_FILES;

/// A `.prev` is kept at most this long after its upgrade (14 days).
pub(crate) const PREV_KEEP_MS: i64 = 1_209_600_000;
/// All `.prev` files together are kept under this size (15 GiB).
pub(crate) const PREV_BUDGET_BYTES: u64 = 16_106_127_360;

/// One `.prev` in the cache and what the rows say of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrevFile {
    pub path: PathBuf,
    pub bytes: u64,
    /// When its upgrade ran (`video_upgrade_at`), else the file's mtime.
    pub since_ms: i64,
    /// A row's video is the file this `.prev` belongs to.
    pub recorded: bool,
    /// The song has `Started` since its upgrade.
    pub played: bool,
}

/// The `.prev` files to delete now and why, in `files`' terms (module doc):
/// orphans, played, expired, then the oldest of the rest while over the
/// budget.
pub(crate) fn plan(files: &[PrevFile], now_ms: i64) -> Vec<(PathBuf, &'static str)> {
    let mut delete = Vec::new();
    let mut kept: Vec<&PrevFile> = Vec::new();
    for file in files {
        let why = if !file.recorded {
            Some("orphan")
        } else if file.played {
            Some("played")
        } else if now_ms - file.since_ms >= PREV_KEEP_MS {
            Some("expired")
        } else {
            None
        };
        match why {
            Some(why) => delete.push((file.path.clone(), why)),
            None => kept.push(file),
        }
    }
    kept.sort_by_key(|file| file.since_ms);
    let mut total: u64 = kept.iter().map(|file| file.bytes).sum();
    for file in kept {
        if total <= PREV_BUDGET_BYTES {
            break;
        }
        total -= file.bytes;
        delete.push((file.path.clone(), "budget"));
    }
    delete
}

/// The video a `.prev` belongs to: its name without `.prev`.
pub(crate) fn video_of(prev: &Path) -> Option<PathBuf> {
    let name = prev.file_name()?.to_str()?;
    let video = name.strip_suffix(".prev")?;
    video
        .ends_with("_video.mp4")
        .then(|| prev.with_file_name(video))
}

/// The `.prev` files in `cache_dir` with what the rows say of each.
pub(crate) async fn gather(pool: &SqlitePool, cache_dir: &Path) -> Vec<PrevFile> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(video) = video_of(&path) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0);
        let row: Option<(Option<i64>, bool)> = match sqlx::query_as(
            "SELECT v.video_upgrade_at, EXISTS ( \
               SELECT 1 FROM play_history h JOIN videos w ON w.id = h.video_id \
               WHERE w.youtube_id = v.youtube_id AND v.video_upgrade_at IS NOT NULL \
                 AND CAST(strftime('%s', h.played_at) AS INTEGER) * 1000 >= v.video_upgrade_at) \
             FROM videos v WHERE v.file_path = ? ORDER BY v.id LIMIT 1",
        )
        .bind(video.to_string_lossy().as_ref())
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                warn!(path = %path.display(), "video upgrade: a .prev's rows could not be read: {e}");
                continue;
            }
        };
        files.push(PrevFile {
            path,
            bytes: meta.len(),
            since_ms: row.and_then(|(at, _)| at).unwrap_or(mtime_ms),
            recorded: row.is_some(),
            played: row.is_some_and(|(_, played)| played),
        });
    }
    files
}

/// Delete what [`plan`] names, under the song-file lock (a rollback reads
/// the same files); how many went.
pub(crate) async fn sweep(pool: &SqlitePool, cache_dir: &Path, now_ms: i64) -> usize {
    let files = gather(pool, cache_dir).await;
    let delete = plan(&files, now_ms);
    if delete.is_empty() {
        return 0;
    }
    let _files = SONG_FILES.lock().await;
    let mut deleted = 0;
    for (path, why) in delete {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                deleted += 1;
                info!(path = %path.display(), why, "video upgrade: the old video is gone");
            }
            Err(e) => warn!(path = %path.display(), "video upgrade: a .prev was not deleted: {e}"),
        }
    }
    deleted
}

/// The `.prev` of `video`, when there is one.
pub(crate) fn prev_of(video: &Path) -> Option<PathBuf> {
    let prev = prev_path(video);
    prev.exists().then_some(prev)
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
