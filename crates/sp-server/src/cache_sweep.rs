//! #261 (no infinite archive): the daily cache sweep. The cache's files no
//! row needs any more go, and the data dir is measured
//! (`crate::data_size`), once a day on the shared daily timer
//! (`crate::daily_pass`), held by the #230 background hold as
//! `Job::CacheSweep`.
//!
//! WHAT goes is decided by the pure `cache_sweep_plan` (its module doc);
//! here: the listing, the rows' facts and the deletes.
//!
//! - The listing (`list`): the cache dir's files, the work dirs of its jobs
//!   (`{id}_isolation` / `_stemsep` / `_dub`, each with its tree's bytes and
//!   newest write), and `<cache>/peer/`'s files. Any other dir (`tools`,
//!   `peer`, `.cli-proxy-api`) is never walked, and a symlink (a junction
//!   on Windows) is never listed, followed or deleted.
//! - The rows: every row's `youtube_id` and its five path columns
//!   (`cache::RECORDING_COLUMNS`, the ones `cache::recorded_by_a_row`
//!   reads), read once.
//! - The pass holds `cache::SONG_FILES` from reading the rows to its last
//!   delete (a rename, a re-link, the retention and a download's record
//!   all take it), so nothing is recorded under a name between the read
//!   and the delete. The listing and the deletes run on the blocking pool.
//! - A delete touches only a name the listing read in the cache dir or in
//!   `peer/` (one plain path component); a file already gone is skipped, a
//!   failed one WARNed and counted. Each delete is one DEBUG line; the pass
//!   is one INFO line with its counts and bytes (DEBUG when it deleted
//!   nothing).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::background_hold::Job;
use crate::cache_sweep_plan::{Entry, Place, Plan, RowFacts, WORK_DIR_RE, Why, plan};
use crate::daily_pass;
use crate::data_size;
use crate::downloader::cache::{self, RECORDING_COLUMNS};

/// The peer exchange's parts dir under the cache (`Exchange::parts_dir`).
pub const PARTS_DIR: &str = "peer";

/// What a pass deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    pub files: u64,
    pub dirs: u64,
    /// The bytes of what went.
    pub bytes: u64,
    /// What went, by reason.
    pub by_why: BTreeMap<Why, u64>,
    /// Deletes that failed (WARNed each).
    pub failed: u64,
    /// Entries due beyond the pass's bound, left for the next day.
    pub left: u64,
}

impl Swept {
    /// How many went for `why`.
    pub fn count(&self, why: Why) -> u64 {
        self.by_why.get(&why).copied().unwrap_or(0)
    }

    /// Whether the pass found nothing to do.
    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.dirs == 0 && self.failed == 0 && self.left == 0
    }
}

/// Every row's id and the files it records, read once.
pub async fn row_facts(pool: &SqlitePool) -> Result<RowFacts, sqlx::Error> {
    type Row = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT youtube_id, {} FROM videos",
        RECORDING_COLUMNS.join(", ")
    ))
    .fetch_all(pool)
    .await?;
    let mut facts = RowFacts::default();
    for (id, a, b, c, d, e) in &rows {
        let paths = [a, b, c, d, e].map(Option::as_deref);
        facts.add_row(id, &paths);
    }
    Ok(facts)
}

/// The cache dir's listing (module doc).
pub fn list(cache_dir: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    list_into(cache_dir, Place::Cache, &mut entries);
    list_into(&cache_dir.join(PARTS_DIR), Place::Parts, &mut entries);
    entries
}

/// The entries of `dir` the sweep looks at: files, and in the cache dir
/// the work dirs (a missing or unreadable dir lists nothing).
fn list_into(dir: &Path, place: Place, entries: &mut Vec<Entry>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        // `DirEntry::metadata` never follows a symlink (or a junction).
        let (Ok(meta), Some(name)) = (
            entry.metadata(),
            entry.file_name().to_str().map(str::to_owned),
        ) else {
            continue;
        };
        let kind = meta.file_type();
        let work_dir = place == Place::Cache && kind.is_dir() && WORK_DIR_RE.is_match(&name);
        let (bytes, modified_ms) = if work_dir {
            let tree = data_size::walk(&entry.path(), None);
            (tree.bytes, tree.newest_ms.filter(|_| tree.complete))
        } else if kind.is_file() {
            (meta.len(), data_size::modified_ms(&meta))
        } else {
            continue;
        };
        entries.push(Entry {
            place,
            name,
            is_dir: work_dir,
            bytes,
            modified_ms,
        });
    }
}

/// Whether `name` is one plain path component (never `..`, a separator or
/// a root): the deletes stay in the dir that was listed.
pub fn is_plain_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_)))
        && parts.next().is_none()
        && !name.contains(['/', '\\'])
}

/// Where a planned entry is.
fn path_of(cache_dir: &Path, place: Place, name: &str) -> PathBuf {
    match place {
        Place::Cache => cache_dir.join(name),
        Place::Parts => cache_dir.join(PARTS_DIR).join(name),
    }
}

/// Delete what `plan` names (module doc).
pub fn execute(cache_dir: &Path, plan: &Plan) -> Swept {
    let mut swept = Swept {
        left: plan.left as u64,
        ..Swept::default()
    };
    for planned in &plan.delete {
        let path = path_of(cache_dir, planned.place, &planned.name);
        if !is_plain_name(&planned.name) {
            warn!(path = %path.display(), "cache sweep: not a plain name - left alone");
            swept.failed += 1;
            continue;
        }
        let deleted = if planned.is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match deleted {
            Ok(()) => {
                if planned.is_dir {
                    swept.dirs += 1;
                } else {
                    swept.files += 1;
                }
                swept.bytes += planned.bytes;
                *swept.by_why.entry(planned.why).or_default() += 1;
                debug!(
                    path = %path.display(),
                    why = planned.why.as_str(),
                    bytes = planned.bytes,
                    "cache sweep: deleted"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                swept.failed += 1;
                warn!(path = %path.display(), "cache sweep: could not delete: {e}");
            }
        }
    }
    swept
}

/// One pass at `now_ms` (module doc).
pub async fn pass(pool: &SqlitePool, cache_dir: &Path, now_ms: i64) -> anyhow::Result<Swept> {
    let _files = cache::SONG_FILES.lock().await;
    let facts = row_facts(pool).await.context("reading the rows")?;
    let dir = cache_dir.to_path_buf();
    tokio::task::spawn_blocking(move || execute(&dir, &plan(&list(&dir), &facts, now_ms)))
        .await
        .context("the sweep's thread")
}

/// The pass's loop on the shared daily timer, until shutdown: the sweep,
/// then the data dir's measure.
#[cfg_attr(test, mutants::skip)] // the timer is daily_pass's; `pass` and the measure are tested
pub async fn run(
    pool: SqlitePool,
    cache_dir: PathBuf,
    data_dir: PathBuf,
    shutdown: broadcast::Receiver<()>,
) {
    daily_pass::run(pool, shutdown, Job::CacheSweep, move |pool, now_ms| {
        let (cache_dir, data_dir) = (cache_dir.clone(), data_dir.clone());
        async move {
            match pass(&pool, &cache_dir, now_ms).await {
                Ok(swept) if swept.is_empty() => debug!("cache sweep: nothing to delete"),
                Ok(swept) => info!(
                    files = swept.files,
                    dirs = swept.dirs,
                    bytes = swept.bytes,
                    no_row = swept.count(Why::NoRow),
                    stale = swept.count(Why::Stale),
                    replaced = swept.count(Why::Replaced),
                    dub_prev = swept.count(Why::DubPrev),
                    parts = swept.count(Why::Part),
                    failed = swept.failed,
                    left = swept.left,
                    "cache sweep: deleted what no row needs any more (#261)"
                ),
                Err(e) => warn!("cache sweep: the pass failed - trying again tomorrow: {e:#}"),
            }
            data_size::refresh(&data_dir, &cache_dir, now_ms).await;
        }
    })
    .await;
}

#[cfg(test)]
#[path = "cache_sweep_tests.rs"]
mod tests;
