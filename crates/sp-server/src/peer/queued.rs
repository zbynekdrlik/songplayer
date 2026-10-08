//! #229: the jobs this node has QUEUED, read from its own rows with each
//! worker's own predicate, never a copy (ROZHODNUTÉ 6022851957 point 2: a
//! peer waits for a queued job as for a running one, `peer::decide`).
//! Queued = the worker would take the row now:
//!
//! - download: `downloader::DOWNLOAD_DUE` (not downloaded, active playlist,
//!   retry due);
//! - lyrics: buckets 1–3 of `lyrics::reprocess` (`lyrics::queue_sql::
//!   queued_where`: manual, null, stale), only while `lyrics_worker_enabled`
//!   is on and this node's paid AI too (#229 item C: `paid_ai`), and
//!   never for a video the catalog will not serve lyrics of (any
//!   row of it dub-requested or Live-Translate, `catalog::LYRICS_ROWS`).
//!   Also a row those buckets take once its own recheck time has come
//!   (`queued_later_where`) whose stems are queued here (the stems'
//!   predicate, the stem worker on; a running job's row still matches it):
//!   a new song's lyrics wait for its stems (`WaitingForStems` puts the row
//!   back for 10 min), and the worker WILL take it once they are done (#229
//!   PP audit, comment 6054582866);
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
    let stems_on = worker_on(pool, STEM_WORKER_ENABLED).await?;
    // #229 item C: lyrics this node may not make (paid AI off) are no job
    // of its: a peer waits for none.
    if worker_on(pool, LYRICS_WORKER_ENABLED).await? && crate::paid_ai::enabled(pool).await {
        let lyrics = lyrics_queued(pool, stems_on).await?;
        add(&mut jobs, lyrics, Job::Lyrics);
    }
    if stems_on {
        let stems: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT youtube_id FROM videos WHERE {STEM_ELIGIBLE_PRED} ORDER BY youtube_id"
        ))
        .fetch_all(pool)
        .await?;
        add(&mut jobs, stems, Job::Stems);
    }
    Ok(jobs)
}

/// The YouTube ids of the lyrics queued here: the rows the lyrics worker
/// takes now, and, while the stem worker is on (`stems_on`), the rows it
/// takes once their recheck time has come whose stems are queued here (a
/// stems wait); each id once, sorted.
async fn lyrics_queued(pool: &SqlitePool, stems_on: bool) -> Result<Vec<String>, sqlx::Error> {
    use crate::lyrics::queue_sql::{queued_later_where, queued_where};
    let mut ids = lyrics_rows(pool, &queued_where()).await?;
    if stems_on {
        // The stems' predicate names no table: inside the subquery its
        // columns are `s`'s, the same row as `v`.
        let waits = format!(
            "{} AND EXISTS (SELECT 1 FROM videos s WHERE s.id = v.id AND {STEM_ELIGIBLE_PRED})",
            queued_later_where()
        );
        ids.extend(lyrics_rows(pool, &waits).await?);
        ids.sort();
        ids.dedup();
    }
    Ok(ids)
}

/// The YouTube ids of the rows `body` takes (a `lyrics::queue_sql` body that
/// binds the current version three times), never a video the catalog will
/// not serve lyrics of.
async fn lyrics_rows(pool: &SqlitePool, body: &str) -> Result<Vec<String>, sqlx::Error> {
    let version = i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION);
    sqlx::query_scalar(&format!(
        "SELECT DISTINCT v.youtube_id FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE {body} AND NOT EXISTS (SELECT 1 FROM videos d WHERE d.youtube_id = v.youtube_id \
             AND (d.dub_requested = 1 OR d.lyrics_source = ?)) \
         ORDER BY v.youtube_id"
    ))
    .bind(version)
    .bind(version)
    .bind(version)
    .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
    .fetch_all(pool)
    .await
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
