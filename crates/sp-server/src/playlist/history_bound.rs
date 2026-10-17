//! #261 (the owner's "nema sa budovat nekonecny archiv!"): `play_history`
//! is bounded once a day, on the shared daily timer (`crate::daily_pass`:
//! 5 min after the start, then daily), held by the #230 background hold as
//! `Job::PlayHistory`.
//!
//! - **One row per playlist and song:** a row with a newer one of the same
//!   playlist and video goes (the newer `played_at`, ties by the later
//!   `id`). The rotation reads only whether a song was played, the stems
//!   tier 2 and the upgrade's `.prev` retention the newest play: none of
//!   them reads another answer.
//! - **[`KEEP_DAYS`]:** an older row goes, except a youtube or dabing
//!   playlist's row of its OWN song: that is its current rotation (the
//!   unplayed list counts the playlist's own plays, and the rotation's
//!   restart clears them), at most one row per song of the playlist. The
//!   30 days cover every other reader: the stems tier 2
//!   (`stems_recent_days`, default 7) and the upgrade's `.prev` retention
//!   (14 days).
//!
//! A custom playlist (ytlive) never restarts a rotation, so its rows used
//! to grow for good.

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::background_hold::Job;
use crate::daily_pass;

/// A play older than this many days goes (outside a current rotation).
pub const KEEP_DAYS: i64 = 30;

/// The oldest `played_at` a pass at `now_ms` keeps, in unix seconds (the
/// column holds whole UTC seconds, `datetime('now')`).
pub fn cutoff_s(now_ms: i64) -> i64 {
    now_ms / 1000 - KEEP_DAYS * 86_400
}

/// What one pass deleted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Bounded {
    /// Rows with a newer row of the same playlist and song.
    pub duplicates: u64,
    /// Rows older than [`KEEP_DAYS`] outside a current rotation.
    pub old: u64,
}

/// A row with a newer one of the same playlist and song.
const DUPLICATES: &str = "DELETE FROM play_history WHERE EXISTS ( \
     SELECT 1 FROM play_history n \
     WHERE n.playlist_id = play_history.playlist_id AND n.video_id = play_history.video_id \
       AND (n.played_at > play_history.played_at \
            OR (n.played_at = play_history.played_at AND n.id > play_history.id)))";

/// A row before the cutoff (the one `?`, unix seconds) that is no youtube
/// or dabing playlist's play of its own song.
const OLD: &str = "DELETE FROM play_history WHERE played_at < datetime(?, 'unixepoch') \
     AND NOT EXISTS ( \
       SELECT 1 FROM videos v JOIN playlists p ON p.id = v.playlist_id \
       WHERE v.id = play_history.video_id AND v.playlist_id = play_history.playlist_id \
         AND p.kind IN ('youtube', 'dabing'))";

/// One pass at `now_ms`: the duplicates, then the old rows.
pub async fn pass(pool: &SqlitePool, now_ms: i64) -> Result<Bounded, sqlx::Error> {
    let duplicates = sqlx::query(DUPLICATES).execute(pool).await?.rows_affected();
    let old = sqlx::query(OLD)
        .bind(cutoff_s(now_ms))
        .execute(pool)
        .await?
        .rows_affected();
    Ok(Bounded { duplicates, old })
}

/// The bound's loop: a pass on the shared daily timer, until shutdown.
#[cfg_attr(test, mutants::skip)] // the timer is daily_pass's; `pass` is tested
pub async fn run(pool: SqlitePool, shutdown: broadcast::Receiver<()>) {
    daily_pass::run(
        pool,
        shutdown,
        Job::PlayHistory,
        |pool, now_ms| async move {
            match pass(&pool, now_ms).await {
                Ok(Bounded {
                    duplicates: 0,
                    old: 0,
                }) => debug!("play history: nothing to delete"),
                Ok(done) => info!(
                    duplicates = done.duplicates,
                    old = done.old,
                    "play history: deleted the older plays of a song and the plays older than \
                 {KEEP_DAYS} days outside a rotation"
                ),
                Err(e) => warn!(%e, "play history: the pass failed - trying again tomorrow"),
            }
        },
    )
    .await;
}

#[cfg(test)]
#[path = "history_bound_tests.rs"]
mod tests;
