//! #229: the jobs this node has QUEUED, read from its own rows with each
//! worker's own predicate, never a copy (ROZHODNUTÉ 6022851957 point 2: a
//! peer is to wait for a queued job as for a running one, lane 7). Queued =
//! the worker would take the row now:
//!
//! - download: `downloader::DOWNLOAD_DUE` (not downloaded, active playlist,
//!   retry due);
//! - lyrics: buckets 1–3 of `lyrics::reprocess` (`queued_where`: manual,
//!   null, stale), only while `lyrics_worker_enabled` is on;
//! - stems: `db::models_stems_priority::STEM_ELIGIBLE_PRED`, only while
//!   `stem_worker_enabled` is on.
//!
//! A YouTube id that is not one is never listed.

use sqlx::SqlitePool;

use super::kind::Job;
use crate::db::models_stems_priority::STEM_ELIGIBLE_PRED;
use crate::downloader::DOWNLOAD_DUE;
use crate::downloader::cache::is_valid_video_id;

/// The lyrics worker's kill switch (`lyrics/worker.rs::process_next`).
const LYRICS_WORKER_ENABLED: &str = "lyrics_worker_enabled";
/// The stem worker's kill switch (`stems/worker.rs`).
const STEM_WORKER_ENABLED: &str = "stem_worker_enabled";

/// Every `(youtube id, job)` this node has queued, downloads first, then
/// lyrics, then stems; each YouTube id once per job.
pub async fn queued(pool: &SqlitePool) -> Result<Vec<(String, Job)>, sqlx::Error> {
    let mut jobs = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();
    let downloads: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT v.youtube_id FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE {DOWNLOAD_DUE} ORDER BY v.youtube_id"
    ))
    .bind(&now)
    .fetch_all(pool)
    .await?;
    add(&mut jobs, downloads, Job::Download);
    if worker_on(pool, LYRICS_WORKER_ENABLED).await? {
        let version = i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION);
        let lyrics: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT v.youtube_id FROM videos v JOIN playlists p ON p.id = v.playlist_id \
             WHERE {} ORDER BY v.youtube_id",
            crate::lyrics::reprocess::queued_where()
        ))
        .bind(version)
        .bind(version)
        .bind(version)
        .fetch_all(pool)
        .await?;
        add(&mut jobs, lyrics, Job::Lyrics);
    }
    if worker_on(pool, STEM_WORKER_ENABLED).await? {
        let stems: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT youtube_id FROM videos WHERE {STEM_ELIGIBLE_PRED} ORDER BY youtube_id"
        ))
        .fetch_all(pool)
        .await?;
        add(&mut jobs, stems, Job::Stems);
    }
    Ok(jobs)
}

/// `job` for every valid YouTube id of `ids`.
fn add(jobs: &mut Vec<(String, Job)>, ids: Vec<String>, job: Job) {
    jobs.extend(
        ids.into_iter()
            .filter(|id| is_valid_video_id(id))
            .map(|id| (id, job)),
    );
}

/// A worker's kill switch as the worker itself reads it (default on).
async fn worker_on(pool: &SqlitePool, setting: &str) -> Result<bool, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, setting).await?;
    Ok(crate::stems::worker::worker_enabled(raw.as_deref()))
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
