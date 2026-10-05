//! A song's title timers (#217 addendum 3). The show timer (`Started` +
//! 1.5 s) pushes the title; the hide timer (3.5 s before the end) takes it
//! down, each only while its scene is on program when it fires (release
//! 0.68.0 blocker 1a) and its playlist owns the wall (#221, release 0.69.0
//! review 🟡 2: while another playlist, or none, owns it, never). They
//! sleep until the instants of the song's `TitleClock`, the same instants a
//! recovery or a scene-on reads (`recovery.rs`), so a `Resync` never
//! contradicts a timer. They used to be armed inline in the `Started`
//! handler; they live here so a scene-on can re-arm them after a scene-off
//! cancelled them. A Play drops the old song's clock and timers
//! (`begin_play`) and re-syncs the wall (`resync_after_play`); a pause
//! cancels the timers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use super::PlaybackEngine;
use super::program_authority::OnAirPlaylists;
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
        let on_air = self.on_air.clone();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        pp.cancel_title_timers();
        let Some(clock) = pp.title_clock.filter(TitleClock::shows) else {
            return;
        };
        let gate = WallGate {
            scene_active: pp.scene_active.clone(),
            on_air,
            playlist_id,
        };
        if clock.show_at > now {
            pp.title_show_abort = Some(spawn_show_timer(
                pool,
                obs_cmd.clone(),
                resolume_tx.clone(),
                gate.clone(),
                clock.video_id,
                clock.show_at,
            ));
        }
        if let Some(hide_at) = clock.hide_at.filter(|hide_at| *hide_at > now) {
            pp.title_hide_abort = Some(spawn_hide_timer(obs_cmd, resolume_tx, gate, hide_at));
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
    /// after its `Started`; if another on-program playlist's title is due,
    /// the Resync names that one. The timers and a `Resync` then never
    /// disagree between the Play and the new `Started`. Off program, or
    /// when it does not own the wall (#221 🟡 2: another one does, or none
    /// does), the playlist's title is not on the wall: nothing is sent. A
    /// pause calls it
    /// too: a paused song's title is not due, and its timers are cancelled
    /// (release 0.68.0 blockers, review round 1).
    pub(super) async fn resync_after_play(&self, playlist_id: i64) {
        let on_program = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| pp.scene_active.load(Ordering::Acquire))
            && self.on_air.may_write_wall(playlist_id);
        if on_program {
            let title = self.resync_wall_title().await;
            debug!(playlist_id, ?title, "title re-synced on play");
        }
    }
}

impl super::PlaylistPipeline {
    /// A Play command starts a song (#217 addendum 3). The last song's title
    /// clock and timers go: a skipped song's pending show timer must not push
    /// its title before the new `Started`, which fixes the new clock, counted
    /// from where `Started` says the song really starts (#217). `start_ms` (0
    /// or a resume's position) is kept as `play_start_ms` (logged next to it)
    /// and is the pause position until the first report. The last
    /// song's lyrics and position go too (release 0.68.0 blockers, review
    /// round 1): a recovery before the new `Started` re-pushed the old song's
    /// line, and a pause there recorded the old song's position for the new.
    /// So does the last pause's resume point (review round 2): a later ▶
    /// resumed the old song over the new one.
    pub(super) fn begin_play(&mut self, start_ms: u64) {
        self.title_clock = None;
        self.cancel_title_timers();
        self.play_start_ms = start_ms;
        self.lyrics_state = None;
        self.cached_position_ms = start_ms;
        self.paused_at = None;
    }
}

/// What a title timer checks when it fires: its playlist's scene is on
/// program, and (#221, release 0.69.0 review 🟡 2) the playlist owns the
/// wall (`OnAirPlaylists::may_write_wall`): while another playlist, or none,
/// owns it, this one's timers never show or hide the shared title.
#[derive(Clone)]
struct WallGate {
    scene_active: Arc<AtomicBool>,
    on_air: OnAirPlaylists,
    playlist_id: i64,
}

impl WallGate {
    /// Whether the timer may write the title now.
    fn open(&self) -> bool {
        self.scene_active.load(Ordering::Acquire) && self.on_air.may_write_wall(self.playlist_id)
    }
}

/// The show timer: at `show_at`, push `video_id`'s title (Resolume, then the
/// OBS text) when its gate is open (read at fire time).
#[cfg_attr(test, mutants::skip)] // spawn glue on the real clock; the deadline is TitleClock's, the arming arm_title_timers's (both unit-tested)
fn spawn_show_timer(
    pool: SqlitePool,
    obs_cmd: Option<mpsc::Sender<ObsCommand>>,
    resolume_tx: mpsc::Sender<ResolumeCommand>,
    gate: WallGate,
    video_id: i64,
    show_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(show_at).await;
        let playlist_id = gate.playlist_id;
        if !gate.open() {
            debug!(
                playlist_id,
                "title suppressed — off program, or the playlist does not own the wall"
            );
            return;
        }
        if title::push_title(&pool, obs_cmd.as_ref(), &resolume_tx, video_id).await {
            info!(playlist_id, video_id, "title shown");
        }
    })
    .abort_handle()
}

/// The hide timer: at `hide_at`, hide the Resolume title and clear the OBS
/// title text (`title::push_hide`) when its gate is open (read at fire time,
/// like the show timer). A playlist off program, e.g. held through a #215
/// transition, must not take down the title of the playlist on program
/// (design record 5863318980 item 1a, `tests_hold.rs`), nor may one on air
/// that does not own the wall (#221 🟡 2, `tests_wall_owner.rs`).
#[cfg_attr(test, mutants::skip)] // spawn glue on the real clock; push_hide is unit-tested, the gate by tests_hold.rs + tests_wall_owner.rs
fn spawn_hide_timer(
    obs_cmd: Option<mpsc::Sender<ObsCommand>>,
    resolume_tx: mpsc::Sender<ResolumeCommand>,
    gate: WallGate,
    hide_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(hide_at).await;
        let playlist_id = gate.playlist_id;
        if !gate.open() {
            debug!(
                playlist_id,
                "title hide suppressed — off program, or the playlist does not own the wall"
            );
            return;
        }
        title::push_hide(obs_cmd.as_ref(), &resolume_tx).await;
        debug!(playlist_id, "title hidden");
    })
    .abort_handle()
}
