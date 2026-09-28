//! A song's title timers (#217 addendum 3). The show timer (`Started` +
//! 1.5 s) pushes the title; the hide timer (3.5 s before the end) takes it
//! down. They sleep until the instants of the song's `TitleClock`, the same
//! instants a recovery or a scene-on reads (`recovery.rs`), so a `Resync`
//! never contradicts a timer. They used to be armed inline in the `Started`
//! handler; they live here so a scene-on can re-arm them after a scene-off
//! cancelled them. A Play drops the old song's clock and timers
//! (`begin_play`) and re-syncs the wall (`resync_after_play`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use super::PlaybackEngine;
use super::title::{self, TitleClock};
use crate::obs::ObsCommand;
use crate::resolume::ResolumeCommand;

impl PlaybackEngine {
    /// Spawn `playlist_id`'s title timers from its song's clock, for the part
    /// of the title window still ahead of `now`: the show timer while the
    /// show point is ahead, the hide timer while the hide point is. The
    /// timers it finds are cancelled first, so no old timer is left to fire.
    /// Called at the song's `Started` (both ahead) and by a scene-on
    /// (`rearm_title_timers`). A clock with no title window (a resume 5 s or
    /// less before the end, `TitleClock::shows`) arms neither: its show
    /// timer would put up a title the window never had (review round 4).
    pub(super) fn arm_title_timers(&mut self, playlist_id: i64, now: Instant) {
        let pool = self.pool.clone();
        let obs_cmd = self.obs_cmd_tx.clone();
        let resolume_tx = self.resolume_tx.clone();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        pp.cancel_title_timers();
        let Some(clock) = pp.title_clock.filter(TitleClock::shows) else {
            return;
        };
        if clock.show_at > now {
            pp.title_show_abort = Some(spawn_show_timer(
                pool,
                obs_cmd.clone(),
                resolume_tx.clone(),
                pp.scene_active.clone(),
                playlist_id,
                clock.video_id,
                clock.show_at,
            ));
        }
        if let Some(hide_at) = clock.hide_at.filter(|hide_at| *hide_at > now) {
            pp.title_hide_abort =
                Some(spawn_hide_timer(obs_cmd, resolume_tx, playlist_id, hide_at));
        }
        debug!(
            playlist_id,
            video_id = clock.video_id,
            show = pp.title_show_abort.is_some(),
            hide = pp.title_hide_abort.is_some(),
            "title timers armed"
        );
    }

    /// A scene-on of `playlist_id` playing `video_id`: arm its title timers
    /// again from its clock. A scene-off cancels them (`handle_scene_change`,
    /// so a timer of a playlist off program never writes the shared clip),
    /// and the #215 transition hold keeps the song playing. Before, a scene
    /// bounce in the song's first 1.5 s left it with no title, and a later
    /// one with no hide 3.5 s before the end. A song this scene-on just
    /// selected has no clock yet (`begin_play`), so nothing is armed: its
    /// `Started` will. The video check is defensive.
    pub(super) fn rearm_title_timers(&mut self, playlist_id: i64, video_id: i64, now: Instant) {
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        if pp
            .title_clock
            .is_none_or(|clock| clock.video_id != video_id)
        {
            return;
        }
        self.arm_title_timers(playlist_id, now);
    }

    /// After a Play of `playlist_id` (review round 4): `begin_play` closed the
    /// old song's title window and cancelled its hide timer, so on program
    /// the wall is re-synced at once. The old song's title goes down now
    /// (an instant hide), not when the new song's ShowTitle replaces it 1.5 s
    /// after its `Started`. The timers and a `Resync` then never disagree
    /// between the Play and the new `Started`. Off program the playlist's
    /// title is not on the wall: nothing is sent.
    pub(super) async fn resync_after_play(&self, _playlist_id: i64) {}
}

impl super::PlaylistPipeline {
    /// A Play command starts a song (#217 addendum 3). The last song's title
    /// clock and timers go: a skipped song's pending show timer must not push
    /// its title before the new `Started`, which fixes the new clock. That
    /// clock counts from `start_ms`, 0 or a resume's position.
    pub(super) fn begin_play(&mut self, start_ms: u64) {
        self.title_clock = None;
        self.cancel_title_timers();
        self.play_start_ms = start_ms;
    }
}

/// The show timer: at `show_at`, push `video_id`'s title (OBS text +
/// Resolume) when the scene is still on program (read at fire time).
#[cfg_attr(test, mutants::skip)] // spawn glue on the real clock; the deadline is TitleClock's, the arming arm_title_timers's (both unit-tested)
fn spawn_show_timer(
    pool: SqlitePool,
    obs_cmd: Option<mpsc::Sender<ObsCommand>>,
    resolume_tx: mpsc::Sender<ResolumeCommand>,
    scene_active: Arc<AtomicBool>,
    playlist_id: i64,
    video_id: i64,
    show_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(show_at).await;
        if !scene_active.load(Ordering::Acquire) {
            debug!(playlist_id, "title suppressed — off program");
            return;
        }
        if title::push_title(&pool, obs_cmd.as_ref(), &resolume_tx, video_id).await {
            info!(playlist_id, video_id, "title shown");
        }
    })
    .abort_handle()
}

/// The hide timer: at `hide_at`, hide the Resolume title and clear the OBS
/// title text (`title::push_hide`).
#[cfg_attr(test, mutants::skip)] // spawn glue on the real clock; push_hide is unit-tested
fn spawn_hide_timer(
    obs_cmd: Option<mpsc::Sender<ObsCommand>>,
    resolume_tx: mpsc::Sender<ResolumeCommand>,
    playlist_id: i64,
    hide_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(hide_at).await;
        title::push_hide(obs_cmd.as_ref(), &resolume_tx).await;
        debug!(playlist_id, "title hidden");
    })
    .abort_handle()
}
