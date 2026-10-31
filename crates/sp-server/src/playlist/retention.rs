//! #260 (the owner's ROZHODNUTÉ, 13.10.2026): a song that left its YouTube
//! playlist is kept [`REMOVED_RETENTION_DAYS`] after its mark — a song put
//! back plays again at once, with its download, stems, lyrics and dub — and
//! then DELETED: the row, its own data and its files.
//!
//! - **Due:** marked at or before [`cutoff_ms`], and in no custom
//!   playlist's set list (`db::models_membership::due_ids`). A song brought
//!   back before that has no mark and is never due.
//! - **Files:** the song's set (`downloader::cache::SongFiles` + the files
//!   named after its audio, `cache::derived_files`), minus every file
//!   another row still records ([`plan_files`]): the same video in another
//!   playlist shares them by name (#136).
//! - **One row:** under `cache::SONG_FILES` (the sync's mark and clear, the
//!   renames and the re-link hold it too), the row is read again, its files
//!   deleted, then the row with its play history and, once no row of the
//!   video is left, the exchange's records of it. A file that cannot be
//!   deleted is a WARN and the row stays for the next pass.
//! - **A pass:** at most [`RETENTION_BATCH`] rows, once after the start and
//!   then once a day ([`run`]); held by the #230 background hold.

use std::collections::HashSet;
use std::path::PathBuf;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::background_hold::Job;
use crate::daily_pass;
use crate::db::models_membership::{self, DueRow};
use crate::downloader::cache::{self, SongFiles};

/// How long a song that left its YouTube playlist is kept (the owner's
/// year; he may ask to change it).
pub const REMOVED_RETENTION_DAYS: i64 = 365;

/// At most this many rows are deleted per pass.
pub const RETENTION_BATCH: i64 = 50;

const DAY_MS: i64 = 86_400_000;

/// The newest mark a pass at `now_ms` deletes: a row marked at or before it
/// has been out of its playlist [`REMOVED_RETENTION_DAYS`] days.
pub fn cutoff_ms(now_ms: i64) -> i64 {
    now_ms - REMOVED_RETENTION_DAYS * DAY_MS
}

/// The files of a deleted row's song, each to be removed or kept.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilePlan {
    /// In this order: the video (a player may hold it open, so its failure
    /// stops the row before anything else is gone), the audio, then the
    /// files named after the audio.
    pub remove: Vec<PathBuf>,
    /// Recorded by another row (or named after an audio another row
    /// records).
    pub keep: Vec<PathBuf>,
}

/// Every file of `files`' set, in [`FilePlan::remove`]'s order: the
/// video, the audio, then the stems, the dub and its transcripts.
pub fn candidates(files: &SongFiles) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(video) = &files.video {
        out.push(video.clone());
    }
    if let Some(audio) = &files.audio {
        out.push(audio.clone());
        out.extend(cache::derived_files(audio));
    }
    out
}

/// What happens to each file of `files` when its row goes: a file in
/// `recorded_elsewhere` (another row records it) is kept, and so is every
/// file named after a kept audio (it is that audio's); the rest is removed.
pub fn plan_files(files: &SongFiles, recorded_elsewhere: &HashSet<PathBuf>) -> FilePlan {
    let audio_kept = files
        .audio
        .as_ref()
        .is_some_and(|audio| recorded_elsewhere.contains(audio));
    let mut plan = FilePlan::default();
    for path in candidates(files) {
        let derived = Some(&path) != files.video.as_ref() && Some(&path) != files.audio.as_ref();
        if recorded_elsewhere.contains(&path) || (derived && audio_kept) {
            plan.keep.push(path);
        } else {
            plan.remove.push(path);
        }
    }
    plan
}

/// One song the retention deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub youtube_id: String,
    pub title: Option<String>,
    /// The files deleted (one already gone is not listed).
    pub removed: Vec<PathBuf>,
    /// The files kept because another row records them (only those on disk).
    pub kept: Vec<PathBuf>,
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub deleted: Vec<Deleted>,
    /// Rows left for the next pass: a file of theirs could not be deleted.
    pub failed: usize,
}

/// One row's outcome.
enum Outcome {
    Deleted(Deleted),
    /// A file could not be deleted (WARNed): the row stays.
    FileFailed,
    /// Not due any more (brought back since the batch was read).
    NotDue,
}

/// One retention pass at `now_ms`: at most [`RETENTION_BATCH`] due rows,
/// each deleted with its files ([`plan_files`]) and its own data, one INFO
/// per deleted song.
pub async fn pass(pool: &SqlitePool, now_ms: i64) -> Result<PassReport, sqlx::Error> {
    let cutoff = cutoff_ms(now_ms);
    let mut report = PassReport::default();
    for id in models_membership::due_ids(pool, cutoff, RETENTION_BATCH).await? {
        match delete_one(pool, id, cutoff).await? {
            Outcome::Deleted(song) => {
                info!(
                    youtube_id = %song.youtube_id,
                    title = song.title.as_deref().unwrap_or(""),
                    removed = ?song.removed,
                    kept = ?song.kept,
                    "playlist retention: deleted a song a year after it left its YouTube playlist"
                );
                report.deleted.push(song);
            }
            Outcome::FileFailed => report.failed += 1,
            Outcome::NotDue => {}
        }
    }
    Ok(report)
}

/// Delete row `id` if it is still due at `cutoff`, with its files and its
/// own data, under `cache::SONG_FILES`.
async fn delete_one(pool: &SqlitePool, id: i64, cutoff: i64) -> Result<Outcome, sqlx::Error> {
    let _files = cache::SONG_FILES.lock().await;
    let Some(row) = models_membership::due_row(pool, id, cutoff).await? else {
        return Ok(Outcome::NotDue);
    };
    let files = SongFiles::recorded(&row.video, row.audio.as_deref());
    let mut elsewhere = HashSet::new();
    for path in candidates(&files) {
        if models_membership::recorded_elsewhere(pool, id, &path).await? {
            elsewhere.insert(path);
        }
    }
    let plan = plan_files(&files, &elsewhere);
    let Some(removed) = remove_files(&row, &plan.remove).await else {
        return Ok(Outcome::FileFailed);
    };
    if !models_membership::delete_row(pool, &row, cutoff, &removed).await? {
        return Ok(Outcome::NotDue);
    }
    let kept = plan.keep.into_iter().filter(|p| p.exists()).collect();
    Ok(Outcome::Deleted(Deleted {
        youtube_id: row.youtube_id,
        title: row.title,
        removed,
        kept,
    }))
}

/// Delete `paths` in order; `None` (one WARN) at the first that cannot be
/// deleted. A file already gone is not an error, and is not listed.
async fn remove_files(row: &DueRow, paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for path in paths {
        match tokio::fs::remove_file(path).await {
            Ok(()) => removed.push(path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(
                    youtube_id = %row.youtube_id,
                    path = %path.display(),
                    %e,
                    "playlist retention: a file was not deleted - the song waits for the next pass"
                );
                return None;
            }
        }
    }
    Some(removed)
}

/// The retention loop: a pass on the shared daily timer
/// (`crate::daily_pass`: 5 min after the start, then once a day, an hour
/// later when the #230 background hold held it), until shutdown.
#[cfg_attr(test, mutants::skip)] // the timer is daily_pass's; `pass` is tested
pub async fn run(pool: SqlitePool, shutdown: broadcast::Receiver<()>) {
    daily_pass::run(pool, shutdown, Job::Retention, |pool, now_ms| async move {
        if let Err(e) = pass(&pool, now_ms).await {
            warn!(%e, "playlist retention: the pass failed - trying again tomorrow");
        }
    })
    .await;
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
