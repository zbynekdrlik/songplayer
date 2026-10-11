//! #223 S12a: the in-place upgrade runs by itself (design comment
//! 6103547599), one song at a time, behind `video_upgrade_enabled` (OFF by
//! default).
//!
//! Every [`TICK`] (the first [`FIRST_TICK`] after the start) one [`tick`]:
//! the gate ([`decide`]: the switch, #230's background hold, a download due,
//! a bot-check pause, [`SPACING_MS`] since the last start, the disk floor),
//! then the pick ([`next_song`]), then S11's [`super::run`]. A failure whose
//! text names YouTube's bot check pauses every upgrade for [`BOT_PAUSE_MS`].
//! `GET /api/v1/video-upgrade` reads [`counts`] and the [`WorkerState`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};

use super::{Outcome, Steps};
use crate::downloader::tools::ToolPaths;

/// The first tick, this long after the start.
pub(crate) const FIRST_TICK: Duration = Duration::from_secs(300);
/// The worker looks again this often.
pub(crate) const TICK: Duration = Duration::from_secs(60);
/// At least this long between two upgrades' starts.
pub(crate) const SPACING_MS: i64 = 120_000;
/// A `busy` song (a player held its file) is tried again after this long.
pub(crate) const BUSY_RETRY_MS: i64 = 600_000;
/// A `failed: …` song is tried again after this long.
pub(crate) const FAILED_RETRY_MS: i64 = 21_600_000;
/// YouTube's bot check pauses every upgrade this long.
pub(crate) const BOT_PAUSE_MS: i64 = 21_600_000;
/// No upgrade while the cache's volume has less free space (50 GiB).
pub(crate) const MIN_FREE_BYTES: u64 = 53_687_091_200;

/// Why a tick ran no upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Skip {
    /// `video_upgrade_enabled` is not `"true"`.
    Off,
    /// #230: the background hold.
    Held,
    /// A new song waits for its download (it comes first).
    DownloadDue,
    /// Inside a bot-check pause.
    Paused,
    /// Less than [`SPACING_MS`] since the last start.
    Spacing,
    /// Less than [`MIN_FREE_BYTES`] free.
    LowDisk,
    /// The free space could not be read.
    NoDiskReading,
    /// The tools are not ready yet.
    NoTools,
    /// Every downloaded song is checked at the live cap.
    NothingToDo,
}

/// What one tick knows before it picks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gate {
    pub enabled: bool,
    pub held: bool,
    pub download_due: bool,
    pub paused_until_ms: Option<i64>,
    pub last_start_ms: Option<i64>,
    pub free_bytes: Option<u64>,
    pub now_ms: i64,
}

/// Whether a tick may pick a song, else why not (in the order checked).
pub(crate) fn decide(gate: &Gate) -> Result<(), Skip> {
    if !gate.enabled {
        return Err(Skip::Off);
    }
    if gate.held {
        return Err(Skip::Held);
    }
    if gate.download_due {
        return Err(Skip::DownloadDue);
    }
    if gate
        .paused_until_ms
        .is_some_and(|until| gate.now_ms < until)
    {
        return Err(Skip::Paused);
    }
    if gate
        .last_start_ms
        .is_some_and(|start| gate.now_ms - start < SPACING_MS)
    {
        return Err(Skip::Spacing);
    }
    match gate.free_bytes {
        None => Err(Skip::NoDiskReading),
        Some(free) if free < MIN_FREE_BYTES => Err(Skip::LowDisk),
        Some(_) => Ok(()),
    }
}

/// Whether a failure's text is YouTube's bot check (sign-in, 429, "try
/// again later").
pub(crate) fn bot_check(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    ["sign in to confirm", "http error 429", "try again later"]
        .iter()
        .any(|mark| error.contains(mark))
}

/// The last upgrade the worker ran.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Last {
    pub youtube_id: String,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub at_ms: i64,
}

/// The worker's memory (a restart forgets it: a pause asks YouTube once
/// more, then pauses again).
#[derive(Debug, Default)]
pub(crate) struct WorkerState {
    pub paused_until_ms: Option<i64>,
    pub last_start_ms: Option<i64>,
    pub last: Option<Last>,
    pub skip: Option<Skip>,
}

/// The process's worker state (the status route reads it).
pub(crate) fn global() -> Arc<Mutex<WorkerState>> {
    static STATE: OnceLock<Arc<Mutex<WorkerState>>> = OnceLock::new();
    STATE.get_or_init(Arc::default).clone()
}

/// The next song to check at `cap` (module doc): downloaded, its check NULL
/// or under the cap, a `busy` one [`BUSY_RETRY_MS`] and a `failed: …` one
/// [`FAILED_RETRY_MS`] after it; songs of active playlists first, then the
/// lowest row id; never the test item.
pub(crate) async fn next_song(
    pool: &SqlitePool,
    cap: u32,
    now_ms: i64,
) -> Result<Option<String>, sqlx::Error> {
    let sql = format!(
        "SELECT v.youtube_id FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.normalized = 1 AND v.file_path IS NOT NULL AND v.audio_file_path IS NOT NULL \
           AND (v.video_upgrade_cap IS NULL OR v.video_upgrade_cap < ?) \
           AND (v.video_upgrade_state IS NULL \
                OR (v.video_upgrade_state = 'busy' AND v.video_upgrade_at <= ?) \
                OR (v.video_upgrade_state LIKE 'failed:%' AND v.video_upgrade_at <= ?) \
                OR (v.video_upgrade_state != 'busy' AND v.video_upgrade_state NOT LIKE 'failed:%')) \
           AND v.{} \
         GROUP BY v.youtube_id ORDER BY MAX(p.is_active) DESC, MIN(v.id) LIMIT 1",
        crate::test_item::not_test_item!()
    );
    sqlx::query_scalar(&sql)
        .bind(i64::from(cap))
        .bind(now_ms - BUSY_RETRY_MS)
        .bind(now_ms - FAILED_RETRY_MS)
        .fetch_optional(pool)
        .await
}

/// Whether a new song waits for its download (`downloader::DOWNLOAD_DUE`).
pub(crate) async fn download_due(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE {})",
        crate::downloader::DOWNLOAD_DUE
    );
    sqlx::query_scalar(&sql)
        .bind(chrono::Utc::now().to_rfc3339())
        .fetch_one(pool)
        .await
}

/// What one tick did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ticked {
    Skipped(Skip),
    Ran(Outcome),
}

/// One tick with `steps` at `now_ms` (module doc); `free_bytes` is the cache
/// volume's free space. The switch is read first: while it is off nothing
/// else is asked (no hold noted, no query).
pub(crate) async fn tick<S: Steps>(
    pool: &SqlitePool,
    cache_dir: &Path,
    state: &Mutex<WorkerState>,
    steps: &S,
    free_bytes: Option<u64>,
    now_ms: i64,
) -> Ticked {
    let lock = || state.lock().unwrap_or_else(PoisonError::into_inner);
    let stored =
        crate::db::models::get_setting(pool, sp_core::config::SETTING_VIDEO_UPGRADE_ENABLED)
            .await
            .ok()
            .flatten();
    let enabled = sp_core::config::video_upgrade_enabled(stored.as_deref());
    let gate = if enabled {
        let (paused_until_ms, last_start_ms) = {
            let s = lock();
            (s.paused_until_ms, s.last_start_ms)
        };
        Gate {
            enabled,
            held: crate::background_hold::holds(pool, crate::background_hold::Job::VideoUpgrade)
                .await,
            download_due: download_due(pool).await.unwrap_or(true),
            paused_until_ms,
            last_start_ms,
            free_bytes,
            now_ms,
        }
    } else {
        Gate {
            enabled,
            held: false,
            download_due: false,
            paused_until_ms: None,
            last_start_ms: None,
            free_bytes,
            now_ms,
        }
    };
    if let Err(skip) = decide(&gate) {
        return Ticked::Skipped(skip);
    }
    let cap = crate::downloader::format::live_cap(pool).await;
    let youtube_id = match next_song(pool, cap, now_ms).await {
        Ok(Some(youtube_id)) => youtube_id,
        Ok(None) => return Ticked::Skipped(Skip::NothingToDo),
        Err(e) => {
            warn!("video upgrade worker: the pick failed: {e}");
            return Ticked::Skipped(Skip::NothingToDo);
        }
    };
    lock().last_start_ms = Some(now_ms);
    info!(youtube_id = %youtube_id, cap, "video upgrade worker: start");
    let report = super::run(pool, cache_dir, &youtube_id, cap, steps, now_ms).await;
    let paused =
        report.outcome == Outcome::Failed && report.error.as_deref().is_some_and(bot_check);
    {
        let mut s = lock();
        if paused {
            s.paused_until_ms = Some(now_ms + BOT_PAUSE_MS);
        }
        s.last = Some(Last {
            youtube_id: youtube_id.clone(),
            outcome: report.outcome,
            error: report.error.clone(),
            at_ms: now_ms,
        });
    }
    match (&report.error, paused) {
        (None, _) => info!(
            youtube_id = %youtube_id,
            outcome = ?report.outcome,
            "video upgrade worker: done"
        ),
        (Some(error), true) => warn!(
            youtube_id = %youtube_id,
            "video upgrade worker: YouTube's bot check - every upgrade paused 6 h: {error}"
        ),
        (Some(error), false) => warn!(
            youtube_id = %youtube_id,
            outcome = ?report.outcome,
            "video upgrade worker: not upgraded: {error}"
        ),
    }
    Ticked::Ran(report.outcome)
}

/// The counts `GET /api/v1/video-upgrade` shows, per YouTube id of the
/// downloaded songs (the test item left out).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub(crate) struct Counts {
    /// Not checked yet at the live cap (a busy or failed one included).
    pub pending: i64,
    pub upgraded: i64,
    pub no_better: i64,
    pub refused: i64,
    pub failed: i64,
    pub busy: i64,
}

/// [`Counts`] at `cap`.
pub(crate) async fn counts(pool: &SqlitePool, cap: u32) -> Result<Counts, sqlx::Error> {
    let sql = format!(
        "SELECT \
           COUNT(*) FILTER (WHERE cap IS NULL OR cap < ? OR state = 'busy' \
                            OR state LIKE 'failed:%') AS pending, \
           COUNT(*) FILTER (WHERE state = 'upgraded') AS upgraded, \
           COUNT(*) FILTER (WHERE state = 'no_better') AS no_better, \
           COUNT(*) FILTER (WHERE state LIKE 'refused:%') AS refused, \
           COUNT(*) FILTER (WHERE state LIKE 'failed:%') AS failed, \
           COUNT(*) FILTER (WHERE state = 'busy') AS busy \
         FROM (SELECT youtube_id, MAX(video_upgrade_cap) AS cap, \
                      MAX(video_upgrade_state) AS state \
               FROM videos WHERE normalized = 1 AND file_path IS NOT NULL AND {} \
               GROUP BY youtube_id)",
        crate::test_item::not_test_item!()
    );
    sqlx::query_as(&sql)
        .bind(i64::from(cap))
        .fetch_one(pool)
        .await
}

/// The worker loop (module doc), until shutdown.
#[cfg_attr(test, mutants::skip)] // the loop and the real steps; `tick` is tested
pub(crate) async fn run(
    pool: SqlitePool,
    cache_dir: PathBuf,
    tool_paths: Arc<RwLock<Option<ToolPaths>>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let state = global();
    let mut wait = FIRST_TICK;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = TICK;
        let tools = tool_paths.read().await.clone();
        let ticked = match tools {
            None => Ticked::Skipped(Skip::NoTools),
            Some(tools) => {
                let steps = super::steps::Real::new(&tools, &cache_dir);
                let free = super::disk::free_bytes(&cache_dir);
                tick(
                    &pool,
                    &cache_dir,
                    &state,
                    &steps,
                    free,
                    crate::peer::wire::now_ms(),
                )
                .await
            }
        };
        if let Ticked::Skipped(skip) = ticked {
            let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
            if s.skip != Some(skip) {
                info!(?skip, "video upgrade worker: waiting");
            } else {
                debug!(?skip, "video upgrade worker: still waiting");
            }
            s.skip = Some(skip);
        } else {
            state.lock().unwrap_or_else(PoisonError::into_inner).skip = None;
        }
    }
    info!("video upgrade worker stopped");
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
