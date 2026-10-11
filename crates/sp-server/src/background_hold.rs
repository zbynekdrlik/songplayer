//! #230 — the owner's rule (`sp_core::background_hold`): a press of the
//! `yt90s` scene (`sp-90s`, ~1.5 min before the service) holds every
//! background job for 4 h; a press of `ytslow`'s (`sp-slow`), or the time,
//! ends it. A job that runs finishes ("blokne ďalšie spracovanie").
//!
//! - The hold is the setting `background_hold_until_ms` (UTC unix ms),
//!   persisted so a restart during the service keeps it, and read live at
//!   every job start ([`holds`]: one DB read, the `paid_ai::enabled`
//!   pattern). An unreadable setting holds nothing (WARN): the hold guards
//!   the service, it never wedges the processing.
//! - [`start`] follows the program bus's on-air watch: every publication
//!   is a cut (a Stream Deck press through the facade, a dashboard cut); one
//!   to `sp-90s` arms the hold from now (a re-press re-arms it), one to
//!   `sp-slow` ends it, and the end instant ends it. The source restored at
//!   startup was published before the watcher subscribed: it presses
//!   nothing.
//! - The jobs that ask, each before it starts one: the playlist sync
//!   (`lib.rs`'s sync consumer drops the request; the periodic sync
//!   re-enqueues), the download, lyrics, stems and dub workers, the metadata
//!   repair, the daily yt-dlp update, and the peer exchange
//!   (`Exchange::transfers_paused`: no fetch, no serving, no hashing).
//!   Startup one-shots and the playback path never ask.
//! - `GET /api/v1/background-hold` ([`status`]) feeds the health bar.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sp_core::background_hold::{
    self as rule, BackgroundHold, HOLD_FOR_S, HOLD_SCENE, Press, RELEASE_SCENE,
    SETTING_BACKGROUND_HOLD_UNTIL,
};
use sqlx::SqlitePool;
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::OnAir;

/// The background jobs that ask the hold before they start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Job {
    Sync,
    Download,
    Lyrics,
    Stems,
    Dub,
    Metadata,
    Peer,
    YtdlpUpdate,
    /// #223 S12: the in-place video upgrade.
    VideoUpgrade,
}

impl Job {
    /// The job's stable name (`held_jobs`, the log's `job`;
    /// `sp_core::background_hold` names it in Slovak).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Download => "download",
            Self::Lyrics => "lyrics",
            Self::Stems => "stems",
            Self::Dub => "dub",
            Self::Metadata => "metadata",
            Self::Peer => "peer",
            Self::YtdlpUpdate => "ytdlp_update",
            Self::VideoUpgrade => "video_upgrade",
        }
    }
}

/// The jobs that found the hold since it was last armed or ended.
static HELD_JOBS: Mutex<BTreeSet<Job>> = Mutex::new(BTreeSet::new());

fn held_jobs() -> std::sync::MutexGuard<'static, BTreeSet<Job>> {
    HELD_JOBS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The stored end; none when unset, mangled or unreadable (WARN).
async fn stored_until(pool: &SqlitePool) -> Option<i64> {
    match crate::db::models::get_setting(pool, SETTING_BACKGROUND_HOLD_UNTIL).await {
        Ok(raw) => rule::parse_until(raw.as_deref()),
        Err(e) => {
            warn!(%e, "background hold: reading {SETTING_BACKGROUND_HOLD_UNTIL} failed - nothing held");
            None
        }
    }
}

/// Whether background jobs are held now.
pub async fn held(pool: &SqlitePool) -> bool {
    rule::held_at(stored_until(pool).await, now_ms())
}

/// Whether `job` must not start now. While held it is noted for the status:
/// ONE INFO per job per hold, then DEBUG.
pub async fn holds(pool: &SqlitePool, job: Job) -> bool {
    if !held(pool).await {
        return false;
    }
    if held_jobs().insert(job) {
        info!(
            job = job.as_str(),
            "background hold: no new job until {RELEASE_SCENE} goes on program or the hold ends"
        );
    } else {
        debug!(job = job.as_str(), "background hold: still held");
    }
    true
}

/// The hold as `GET /api/v1/background-hold` shows it.
pub async fn status(pool: &SqlitePool) -> BackgroundHold {
    let until = stored_until(pool).await;
    let now = now_ms();
    let held = rule::held_at(until, now);
    BackgroundHold {
        held,
        until_utc_ms: until,
        remaining_s: rule::remaining_s(until, now),
        hold_scene: HOLD_SCENE.to_string(),
        release_scene: RELEASE_SCENE.to_string(),
        held_jobs: if held {
            held_jobs().iter().map(|j| j.as_str().to_string()).collect()
        } else {
            Vec::new()
        },
    }
}

/// Hold from `now` for `hold_for`: the end stored, the held jobs forgotten.
/// `None` (WARN) when it could not be stored.
async fn arm(pool: &SqlitePool, now: i64, hold_for: Duration) -> Option<i64> {
    let until = now.saturating_add(i64::try_from(hold_for.as_millis()).unwrap_or(i64::MAX));
    let stored =
        crate::db::models::set_setting(pool, SETTING_BACKGROUND_HOLD_UNTIL, &until.to_string())
            .await;
    if let Err(e) = stored {
        warn!(%e, "background hold: {HOLD_SCENE} went on program but the hold was not stored - nothing held");
        return None;
    }
    held_jobs().clear();
    info!(
        until_utc = %chrono::DateTime::from_timestamp_millis(until).map(|t| t.to_rfc3339()).unwrap_or_default(),
        "background hold: on - {HOLD_SCENE} went on program; no new background job until \
         {RELEASE_SCENE} goes on program or the hold ends (a running job finishes)"
    );
    Some(until)
}

/// End the hold (`why` for the log): the end deleted, the held jobs
/// forgotten. INFO when it was holding or `timed` (its end instant came: a
/// read at or after the end no longer counts it as held), DEBUG for a
/// stale end.
async fn release(pool: &SqlitePool, why: &str, timed: bool) {
    let was_held = held(pool).await;
    let deleted = sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(SETTING_BACKGROUND_HOLD_UNTIL)
        .execute(pool)
        .await;
    if let Err(e) = deleted {
        warn!(%e, why, "background hold: ending the hold failed - it ends at its stored time");
        return;
    }
    held_jobs().clear();
    if was_held || timed {
        info!(why, "background hold: off - background jobs start again");
    } else {
        debug!(why, "background hold: a stale end deleted");
    }
}

/// How long a hold ending at `until` still holds at `now`: `None` when
/// nothing holds (no end, or one already past: it holds nothing, and a
/// delete that failed is never retried in a spin).
fn time_left(until: Option<i64>, now: i64) -> Option<Duration> {
    let left = until?.saturating_sub(now);
    (left > 0).then(|| Duration::from_millis(left.unsigned_abs()))
}

/// Follow `bus`'s on-air watch until shutdown (the module doc).
#[cfg_attr(test, mutants::skip)]
pub fn start(pool: SqlitePool, bus: &Arc<ProgramBus>, shutdown: &broadcast::Sender<()>) {
    let hold_for = Duration::from_secs(HOLD_FOR_S);
    tokio::spawn(run(pool, bus.on_air(), hold_for, shutdown.subscribe()));
}

/// The watcher: each publication of `on_air` after the subscription is a
/// cut; `hold_for` is a parameter so a test can see the end pass.
pub(crate) async fn run(
    pool: SqlitePool,
    mut on_air: watch::Receiver<OnAir>,
    hold_for: Duration,
    mut shutdown: broadcast::Receiver<()>,
) {
    loop {
        let left = time_left(stored_until(&pool).await, now_ms());
        tokio::select! {
            _ = shutdown.recv() => return,
            changed = on_air.changed() => {
                if changed.is_err() {
                    return;
                }
                let scene = on_air.borrow_and_update().scene.clone();
                match rule::press_of(scene.as_deref()) {
                    Press::Hold => {
                        arm(&pool, now_ms(), hold_for).await;
                    }
                    Press::Release => {
                        if stored_until(&pool).await.is_some() {
                            release(&pool, "release scene on program", false).await;
                        }
                    }
                    Press::Other => {}
                }
            }
            _ = tokio::time::sleep(left.unwrap_or_default()), if left.is_some() => {
                release(&pool, "hold time passed", true).await;
            }
        }
    }
}

/// Tests: hold `pool`'s node for a minute from now.
#[cfg(test)]
pub(crate) async fn hold_for_a_minute(pool: &SqlitePool) {
    let until = (now_ms() + 60_000).to_string();
    crate::db::models::set_setting(pool, SETTING_BACKGROUND_HOLD_UNTIL, &until)
        .await
        .unwrap();
}

/// Tests: end `pool`'s hold (the stored end deleted; the process-wide
/// held-job set is left alone, so a parallel test reading it is not
/// disturbed).
#[cfg(test)]
pub(crate) async fn end_hold(pool: &SqlitePool) {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(SETTING_BACKGROUND_HOLD_UNTIL)
        .execute(pool)
        .await
        .unwrap();
}

#[cfg(test)]
#[path = "background_hold_tests.rs"]
mod tests;
