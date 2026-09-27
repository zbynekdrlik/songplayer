//! A song's title timers (#217 addendum 3). The show timer (`Started` +
//! 1.5 s) pushes the title; the hide timer (3.5 s before the end) takes it
//! down. They sleep until the instants of the song's `TitleClock`, the same
//! instants a recovery or a scene-on reads (`recovery.rs`), so a `Resync`
//! never contradicts a timer. They used to be armed inline in the `Started`
//! handler; they live here so a scene-on can re-arm them after a scene-off
//! cancelled them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use super::PlaybackEngine;
use super::title::{self, OBS_TITLE_SOURCE};
use crate::obs::ObsCommand;
use crate::resolume::ResolumeCommand;

impl PlaybackEngine {
    /// Spawn `playlist_id`'s title timers from its song's clock, for the part
    /// of the title window still ahead of `now`: the show timer while the
    /// show point is ahead, the hide timer while the hide point is. The
    /// timers it finds are cancelled first, so no old timer is left to fire.
    /// Called at the song's `Started` (both ahead) and by a scene-on
    /// (`rearm_title_timers`).
    pub(super) fn arm_title_timers(&mut self, playlist_id: i64, now: Instant) {
        let pool = self.pool.clone();
        let obs_cmd = self.obs_cmd_tx.clone();
        let resolume_tx = self.resolume_tx.clone();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        pp.cancel_title_timers();
        let Some(clock) = pp.title_clock else {
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
    /// one with no hide 3.5 s before the end. A clock of another video (a
    /// song this scene-on just selected, not started yet) arms nothing: its
    /// `Started` will.
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

/// The hide timer: at `hide_at`, clear the OBS title text and hide the
/// Resolume title.
#[cfg_attr(test, mutants::skip)] // spawn glue on the real clock (see spawn_show_timer)
fn spawn_hide_timer(
    obs_cmd: Option<mpsc::Sender<ObsCommand>>,
    resolume_tx: mpsc::Sender<ResolumeCommand>,
    playlist_id: i64,
    hide_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(hide_at).await;
        if let Some(cmd_tx) = obs_cmd {
            let _ = cmd_tx
                .send(ObsCommand::SetTextSource {
                    source_name: OBS_TITLE_SOURCE.to_string(),
                    text: String::new(),
                })
                .await;
        }
        let _ = resolume_tx.send(ResolumeCommand::HideTitle).await;
        debug!(playlist_id, "title hidden");
    })
    .abort_handle()
}
