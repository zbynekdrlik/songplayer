//! #223 S11: the swap of a song's video (design comment 6103060545 step 6).
//!
//! Under [`SONG_FILES`] (the lock every song-file rename takes, #136): every
//! row of the video that names a video file must still name the checked
//! one; then the old file is hard-linked as `<name>.prev` and the temp is
//! renamed over the name. One replacing rename, so the name never lacks a
//! video; a player that holds the file either refuses the rename (nothing
//! changed, `Busy`) or keeps reading the old data, which `.prev` still
//! holds.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::downloader::cache::SONG_FILES;

/// How a swap ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Swapped {
    /// The new video has the old one's name; the old one is `<name>.prev`.
    Done,
    /// The rename was refused (a player holds the file): nothing changed.
    Busy(String),
    /// The swap did not start: the rows moved on, or the link failed.
    Refused(String),
}

/// The old video's name while S12 keeps it: `<name>.prev`.
pub(crate) fn prev_path(video: &Path) -> PathBuf {
    let mut name = video.as_os_str().to_os_string();
    name.push(".prev");
    PathBuf::from(name)
}

/// Swap `temp` in as `youtube_id`'s video `checked` (module doc).
pub(crate) async fn swap(
    pool: &SqlitePool,
    youtube_id: &str,
    checked: &Path,
    temp: &Path,
) -> Swapped {
    let _files = SONG_FILES.lock().await;
    match rows_name_only(pool, youtube_id, checked).await {
        Ok(true) => replace(checked, temp),
        Ok(false) => Swapped::Refused(format!(
            "the rows of the video no longer name {} alone",
            checked.display()
        )),
        Err(e) => Swapped::Refused(format!("the rows: {e}")),
    }
}

/// Whether every row of the video that names a video file names `checked`
/// (and one does).
async fn rows_name_only(
    pool: &SqlitePool,
    youtube_id: &str,
    checked: &Path,
) -> Result<bool, sqlx::Error> {
    let paths: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT file_path FROM videos WHERE youtube_id = ? AND file_path IS NOT NULL",
    )
    .bind(youtube_id)
    .fetch_all(pool)
    .await?;
    Ok(!paths.is_empty() && paths.iter().all(|(path,)| Path::new(path) == checked))
}

/// The file half of the swap: `video` linked as `.prev` (a stale one
/// removed first), then `temp` renamed over `video`. A refused rename
/// removes the link again.
pub(crate) fn replace(video: &Path, temp: &Path) -> Swapped {
    let prev = prev_path(video);
    match std::fs::remove_file(&prev) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Swapped::Refused(format!("the stale {}: {e}", prev.display())),
    }
    if let Err(e) = std::fs::hard_link(video, &prev) {
        return Swapped::Refused(format!("the link {}: {e}", prev.display()));
    }
    match std::fs::rename(temp, video) {
        Ok(()) => Swapped::Done,
        Err(e) => {
            let _ = std::fs::remove_file(&prev);
            Swapped::Busy(format!("the rename over {}: {e}", video.display()))
        }
    }
}

#[cfg(test)]
#[path = "swap_tests.rs"]
mod tests;
