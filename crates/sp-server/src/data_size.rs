//! #261 (no infinite archive): how much the data dir holds, measured after
//! each daily cache sweep (`cache_sweep::run`) and read by
//! `GET /api/v1/status` (`sp_core::data_size::DataSize`: null until the
//! first measure). The walk never follows a symlink (a junction on
//! Windows), so it never counts a file outside the dir twice or loops.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use sp_core::data_size::DataSize;

/// What a walk of a dir found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tree {
    /// The bytes of every file in it.
    pub bytes: u64,
    /// The newest last write in it, the dir's own included, UTC ms.
    pub newest_ms: Option<i64>,
    /// Every entry could be read.
    pub complete: bool,
    /// The walk met the dir it was told to skip.
    pub skipped: bool,
}

/// A file's last write, UTC ms (`None` when the system cannot tell).
pub fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let at = meta.modified().ok()?;
    let since = at.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Walk `dir` and everything in it, symlinks left out and `skip` (a dir in
/// it) left out whole. Each entry's facts come from the directory listing
/// (cheap; on NTFS updated lazily for a file still open).
pub fn walk(dir: &Path, skip: Option<&Path>) -> Tree {
    walk_with(dir, skip, false)
}

/// [`walk`], every entry read from its own record (`symlink_metadata`, one
/// open each), never NTFS's lazily updated directory index: the cache
/// sweep's work dirs, where a job may still be writing.
pub fn walk_exact(dir: &Path) -> Tree {
    walk_with(dir, None, true)
}

/// [`walk`] / [`walk_exact`].
fn walk_with(dir: &Path, skip: Option<&Path>, exact: bool) -> Tree {
    let mut tree = Tree {
        complete: true,
        ..Tree::default()
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&next) else {
            tree.complete = false;
            continue;
        };
        tree.newest_ms = tree.newest_ms.max(modified_ms(&meta));
        let Ok(entries) = std::fs::read_dir(&next) else {
            tree.complete = false;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                tree.complete = false;
                continue;
            };
            let path = entry.path();
            if skip.is_some_and(|s| s == path) {
                tree.skipped = true;
                continue;
            }
            // Neither follows a symlink (or a junction).
            let meta = if exact {
                std::fs::symlink_metadata(&path)
            } else {
                entry.metadata()
            };
            let Ok(meta) = meta else {
                tree.complete = false;
                continue;
            };
            let kind = meta.file_type();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            } else {
                tree.bytes = tree.bytes.saturating_add(meta.len());
                tree.newest_ms = tree.newest_ms.max(modified_ms(&meta));
            }
        }
    }
    tree
}

/// `(data dir bytes, cache bytes)`: the cache walked once, counted into the
/// data dir when it lies in it.
pub fn measure(data_dir: &Path, cache_dir: &Path) -> (u64, u64) {
    let cache = walk(cache_dir, None).bytes;
    let rest = walk(data_dir, Some(cache_dir));
    let data = if rest.skipped {
        rest.bytes.saturating_add(cache)
    } else {
        rest.bytes
    };
    (data, cache)
}

/// The last measure (null until the first).
static MEASURED: Mutex<DataSize> = Mutex::new(DataSize {
    data_dir_bytes: None,
    cache_bytes: None,
    data_dir_measured_ms: None,
});

/// The last measure, for `GET /api/v1/status`.
pub fn current() -> DataSize {
    *MEASURED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keep `size` as the last measure.
pub fn set(size: DataSize) {
    *MEASURED.lock().unwrap_or_else(PoisonError::into_inner) = size;
}

/// Measure the data dir now (on the blocking pool), keep it and log it.
#[cfg_attr(test, mutants::skip)] // `measure` and the store are tested; this joins them
pub async fn refresh(data_dir: &Path, cache_dir: &Path, now_ms: i64) {
    let (data, cache) = (data_dir.to_path_buf(), cache_dir.to_path_buf());
    match tokio::task::spawn_blocking(move || measure(&data, &cache)).await {
        Ok((data_dir_bytes, cache_bytes)) => {
            set(DataSize {
                data_dir_bytes: Some(data_dir_bytes),
                cache_bytes: Some(cache_bytes),
                data_dir_measured_ms: Some(now_ms),
            });
            tracing::info!(data_dir_bytes, cache_bytes, "data dir: measured");
        }
        Err(e) => tracing::warn!(%e, "data dir: the measure failed"),
    }
}

#[cfg(test)]
#[path = "data_size_tests.rs"]
mod tests;
