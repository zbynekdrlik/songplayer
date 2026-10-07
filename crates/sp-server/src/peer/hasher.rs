//! #229: the sha256 cache behind the catalog. A file is hashed once per
//! (path, size, mtime), one file at a time, at ≤ [`HASH_BYTES_PER_S`], stat →
//! hash → stat (a file that changed meanwhile is skipped until the next
//! pass), only while this node serves and transfers are not paused. A file
//! whose row is gone or renamed loses its entry; a missing file too. The
//! first pass over SNV's ~115 GB takes ~50 min; later passes hash only new
//! files.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::broadcast;
use tracing::{info, warn};

use super::Exchange;
use super::catalog::{ArtifactFile, artifact_files, path_key};
use super::config::NodeConfig;
use super::hash::sha256_file;
use super::wire::now_ms;
use crate::db::models_peer::{self, HashEntry};

/// The hasher's read rate: the wall reads its video from the same disk.
pub const HASH_BYTES_PER_S: u64 = 40 * 1024 * 1024;
/// Between passes (the first pass waits too: the 60 s startup quiet, #167).
const PASS_EVERY: Duration = Duration::from_secs(60);

/// What one pass did, per file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HashPass {
    /// Hashed and stored.
    pub hashed: usize,
    /// Its entry still holds (same size and mtime).
    pub fresh: usize,
    /// Not on disk or unreadable: its entry is dropped.
    pub missing: usize,
    /// It changed while being hashed: not stored, the next pass tries again.
    pub changed: usize,
    /// Entries of paths no row names any more, dropped.
    pub pruned: u64,
    /// The pass stopped at a pause.
    pub paused: bool,
}

/// What became of one file in a pass.
enum Outcome {
    Fresh,
    Missing,
    Changed,
    Paused,
    Hashed(HashEntry),
}

/// One pass over this node's artifact files at `rate` bytes/s (0 = no limit).
pub async fn hash_pass(ex: &Exchange, rate: u64) -> Result<HashPass, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let keep: HashSet<String> = files.iter().map(|f| path_key(&f.path)).collect();
    let mut pass = HashPass::default();
    for f in &files {
        match one_file(ex, f, &hashes, rate).await {
            Outcome::Fresh => pass.fresh += 1,
            Outcome::Missing => {
                pass.missing += 1;
                models_peer::remove_hash(&ex.pool, &path_key(&f.path)).await?;
            }
            Outcome::Changed => pass.changed += 1,
            Outcome::Paused => {
                pass.paused = true;
                break;
            }
            Outcome::Hashed(entry) => {
                models_peer::put_hash(&ex.pool, &entry).await?;
                pass.hashed += 1;
            }
        }
    }
    pass.pruned = models_peer::prune_hashes(&ex.pool, &keep).await?;
    Ok(pass)
}

/// stat → (fresh? paused?) → hash → stat for one file.
async fn one_file(
    ex: &Exchange,
    f: &ArtifactFile,
    hashes: &HashMap<String, HashEntry>,
    rate: u64,
) -> Outcome {
    let key = path_key(&f.path);
    let Some((size, mtime_ms)) = stat(&f.path).await else {
        return Outcome::Missing;
    };
    if hashes
        .get(&key)
        .is_some_and(|h| h.size == size && h.mtime_ms == mtime_ms)
    {
        return Outcome::Fresh;
    }
    if ex.transfers_paused().await {
        return Outcome::Paused;
    }
    let Ok(sha256) = sha256_file(&f.path, rate).await else {
        return Outcome::Missing;
    };
    if stat(&f.path).await != Some((size, mtime_ms)) {
        return Outcome::Changed;
    }
    Outcome::Hashed(HashEntry {
        path: key,
        size,
        mtime_ms,
        sha256,
        hashed_at_ms: now_ms(),
    })
}

/// `(size, mtime ms)` of a file, `None` when it cannot be read.
async fn stat(path: &Path) -> Option<(i64, i64)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((
        i64::try_from(meta.len()).ok()?,
        i64::try_from(mtime.as_millis()).ok()?,
    ))
}

/// The hasher runs only while this node serves and transfers are not paused.
pub(crate) async fn should_hash(ex: &Exchange) -> bool {
    NodeConfig::load(&ex.pool).await.is_ok_and(|c| c.serving()) && !ex.transfers_paused().await
}

/// The hasher's loop: a pass every [`PASS_EVERY`] while [`should_hash`].
/// A pass that hashed, found changed or pruned anything is logged at INFO; a
/// pass that only finds the same missing files as the last one is not.
#[cfg_attr(test, mutants::skip)] // the 60 s timer loop around hash_pass +
// should_hash (both tested); a terminating test can only watch it exit, the
// same class as ReprocessWorker::run.
pub async fn run(ex: Arc<Exchange>, mut shutdown: broadcast::Receiver<()>) {
    info!("exchange: hasher started");
    let mut last_missing = 0;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(PASS_EVERY) => {}
        }
        if !should_hash(&ex).await {
            continue;
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            pass = hash_pass(&ex, HASH_BYTES_PER_S) => match pass {
                Ok(p) => {
                    let busy = p.hashed + p.changed > 0 || p.pruned > 0;
                    if busy || p.missing != last_missing {
                        info!(
                            hashed = p.hashed,
                            fresh = p.fresh,
                            missing = p.missing,
                            changed = p.changed,
                            pruned = p.pruned,
                            paused = p.paused,
                            "exchange: hashing pass"
                        );
                    }
                    last_missing = p.missing;
                }
                Err(e) => warn!(%e, "exchange: hashing pass failed"),
            },
        }
    }
    info!("exchange: hasher stopped");
}

#[cfg(test)]
#[path = "hasher_tests.rs"]
mod tests;
