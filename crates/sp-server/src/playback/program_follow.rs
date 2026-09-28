//! `SP-program` follows cg OBS natively, and the transition every program cut
//! uses (#215, B5 of EPIC #174). Design records: #215 comment 5853036223; the
//! follow as a pure consumer of the OBS client's state, #219 comment
//! 5868318993.
//!
//! - **Transition spec.** The task keeps the program bus's spec
//!   (`ProgramBus::set_transition`) in step with the operator's settings and cg
//!   OBS's current scene transition. `program_transition` = `obs` (the default)
//!   uses cg OBS's transition: `fade_transition` → a Fade of its duration,
//!   `cut_transition` → Cut, any other kind → a Fade of its duration. `fade` /
//!   `cut` override it, and `program_transition_ms` is the fade length when
//!   SongPlayer picks it (`program_transition::effective_spec`). Every cut
//!   (dashboard, #213 remote control, this follow) uses that spec.
//! - **cg OBS's state comes from the OBS client** (#219): the task consumes the
//!   client's published [`ObsSnapshot`]s (a `watch`, `obs::snapshot`) and asks
//!   cg OBS nothing itself. cg OBS's transition is the snapshot's (the client
//!   reads it at connect and on cg OBS's transition events,
//!   `obs::transition`); the last known one is kept while a snapshot has none.
//! - **Follow** (`program_follow_obs`, off by default). When the program scene
//!   of a snapshot CHANGES (its name or its playlists — the client's
//!   `check_scene_items` over the same `NdiSourceMap` the #213 facade uses,
//!   with the #170 poll repairing an event cg OBS dropped), the scene is mapped
//!   with `remote::map::scene_action`, the #213 rule: exactly one playlist →
//!   that playlist; otherwise "OBS manuál" while it is a source; else keep.
//!   The cut then goes through `program_bus::persist_and_cut`; a scene that
//!   maps to the on-program source cuts nothing.
//!   - A snapshot whose playlist lookup FAILED (#218) is ignored: its
//!     playlists belong to an earlier scene. The client's repaired lookup is
//!     the next change.
//!   - A disconnected (or not yet named) snapshot forgets the scene, so the
//!     reconnect's scene is followed again.
//!   - At start and when the follow is switched on, the current snapshot's
//!     scene is followed whether it changed or not (the catch-up).
//!   - A snapshot that changes only the transition applies the spec and cuts
//!     nothing.
//!
//!   This replaces the event-night watcher script `%TEMP%\sp_follow.ps1`,
//!   which polled the scene.
//! - The settings are re-read every [`FOLLOW_SETTINGS_POLL`], so a save
//!   applies within 5 s. The telemetry ([`FollowShared`], on
//!   `ProgramBus::follow()`) is served as `follow` on `GET /api/v1/program`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use sp_core::config::{
    SETTING_PROGRAM_FOLLOW_OBS, SETTING_PROGRAM_TRANSITION, SETTING_PROGRAM_TRANSITION_MS,
    program_transition_ms,
};
use sqlx::SqlitePool;
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use crate::obs::ObsSnapshot;
use crate::playback::program_bus::{ProgramBus, persist_and_cut};
use crate::playback::program_transition::{
    ObsTransition, TransitionMode, TransitionSpec, effective_spec,
};
use crate::remote::map::{SceneAction, scene_action};
use crate::remote::{RemoteCut, clip};

/// How often the task re-reads the settings.
pub const FOLLOW_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The stored follow + transition settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FollowSettings {
    /// `program_follow_obs`: only `"true"` follows.
    pub follow_obs: bool,
    /// `program_transition`.
    pub mode: TransitionMode,
    /// `program_transition_ms`.
    pub ms: u32,
}

impl Default for FollowSettings {
    /// Off, following cg OBS's transition, 300 ms (every setting absent).
    fn default() -> Self {
        Self {
            follow_obs: false,
            mode: TransitionMode::Obs,
            ms: sp_core::config::DEFAULT_PROGRAM_TRANSITION_MS,
        }
    }
}

/// Read the follow + transition settings.
pub async fn load_follow_settings(pool: &SqlitePool) -> Result<FollowSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let follow_obs = get_setting(pool, SETTING_PROGRAM_FOLLOW_OBS)
        .await?
        .is_some_and(|v| v.trim() == "true");
    let mode = TransitionMode::parse(
        get_setting(pool, SETTING_PROGRAM_TRANSITION)
            .await?
            .as_deref(),
    );
    let ms = program_transition_ms(
        get_setting(pool, SETTING_PROGRAM_TRANSITION_MS)
            .await?
            .as_deref(),
    );
    Ok(FollowSettings {
        follow_obs,
        mode,
        ms,
    })
}

/// `GET /api/v1/program` → `follow`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FollowStatus {
    /// From the STORED settings (a save shows at once).
    pub enabled: bool,
    pub mode: TransitionMode,
    pub ms: u32,
    /// cg OBS's current scene transition, `None` until the OBS client knew it.
    pub obs_transition: Option<ObsTransition>,
    /// The last scene change the program followed (the #213 remote-cut shape).
    pub last_follow_cut: Option<RemoteCut>,
}

#[derive(Default)]
struct FollowState {
    obs: Option<ObsTransition>,
    last_cut: Option<RemoteCut>,
}

/// The follow task's telemetry, shared with the API through
/// `ProgramBus::follow()`. Every method holds its lock for µs only.
#[derive(Default)]
pub struct FollowShared {
    state: Mutex<FollowState>,
}

impl FollowShared {
    fn state(&self) -> MutexGuard<'_, FollowState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `follow` block for the stored `settings`.
    pub fn status(&self, settings: &FollowSettings) -> FollowStatus {
        let st = self.state();
        FollowStatus {
            enabled: settings.follow_obs,
            mode: settings.mode,
            ms: settings.ms,
            obs_transition: st.obs.clone(),
            last_follow_cut: st.last_cut.clone(),
        }
    }

    /// cg OBS's last known transition.
    pub fn obs_transition(&self) -> Option<ObsTransition> {
        self.state().obs.clone()
    }

    /// Store cg OBS's transition; `true` when it changed.
    pub fn set_obs_transition(&self, obs: ObsTransition) -> bool {
        let mut st = self.state();
        let changed = st.obs.as_ref() != Some(&obs);
        st.obs = Some(obs);
        changed
    }

    fn record_cut(&self, cut: RemoteCut) {
        self.state().last_cut = Some(cut);
    }
}

/// What the task works on: the settings store and the program bus.
pub struct Follow {
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
}

impl Follow {
    pub fn new(pool: SqlitePool, bus: Arc<ProgramBus>) -> Self {
        Self { pool, bus }
    }

    /// The stored settings; an unreadable store keeps `current`.
    pub async fn load(&self, current: FollowSettings) -> FollowSettings {
        match load_follow_settings(&self.pool).await {
            Ok(settings) => settings,
            Err(e) => {
                warn!(%e, "program follow: reading the settings failed — keeping the last ones");
                current
            }
        }
    }

    /// Put the spec of `settings` + cg OBS's last known transition on the bus
    /// and return it; a change is logged.
    pub fn apply_spec(&self, settings: &FollowSettings) -> TransitionSpec {
        let obs = self.bus.follow().obs_transition();
        let spec = effective_spec(settings.mode, settings.ms, obs.as_ref());
        if self.bus.set_transition(spec) {
            info!(
                kind = ?spec.kind,
                duration_ms = spec.duration_ms,
                n_slots = spec.n_slots,
                source = ?spec.source,
                "program transition: every cut now uses this transition"
            );
        }
        spec
    }

    /// Follow one cg OBS program scene: map it with the #213 rule
    /// (`scene_action`) and cut `SP-program` through `persist_and_cut`, unless
    /// it already shows that source. Records the outcome as `last_follow_cut`.
    pub async fn follow_scene(&self, scene: &str, playlists: &HashSet<i64>) -> SceneAction {
        let input_active = crate::playback::ndi_input::load_input_settings(&self.pool)
            .await
            .is_ok_and(|s| s.active());
        let action = scene_action(Some(playlists), input_active);
        let mut reason = action.keep_reason().map(|r| r.as_str());
        let mut cut_boundary_100ns = None;
        match action.source() {
            Some(source) if self.bus.status().source != Some(source) => {
                match persist_and_cut(&self.pool, &self.bus, source).await {
                    Ok(status) => {
                        cut_boundary_100ns = status.cut_boundary_100ns;
                        info!(
                            scene = %clip(scene),
                            source,
                            cut_boundary_100ns = ?status.cut_boundary_100ns,
                            "program follow: cut to the cg OBS program scene"
                        );
                    }
                    Err(e) => {
                        reason = Some("persist_failed");
                        warn!(%e, source, "program follow: persisting the source failed — nothing cut");
                    }
                }
            }
            Some(_) => {} // already on program
            None => warn!(
                scene = %clip(scene),
                reason = ?reason,
                "program follow: the scene maps to no program source — the program stays"
            ),
        }
        self.bus.follow().record_cut(RemoteCut {
            scene: clip(scene),
            action: action.label(),
            source: action.source(),
            reason,
            cut_boundary_100ns,
            at_ms: chrono::Utc::now().timestamp_millis(),
        });
        action
    }
}

/// Start the task (called once from `PlaybackEngine::start_program`) on the
/// OBS client's snapshots (a closed channel when OBS is not configured).
#[cfg_attr(test, mutants::skip)] // orchestration glue; the task itself is tested
pub fn start_follow(
    follow: Follow,
    obs: watch::Receiver<ObsSnapshot>,
    shutdown: &broadcast::Sender<()>,
) {
    let rx = shutdown.subscribe();
    tokio::spawn(run_follow_task(follow, obs, rx, FOLLOW_SETTINGS_POLL));
}

/// A program scene with its playlists, as a snapshot reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SceneView {
    scene: String,
    playlists: HashSet<i64>,
}

/// cg OBS's program scene as a snapshot tells it (#219).
#[derive(Debug, PartialEq, Eq)]
enum ProgramScene {
    /// Connected, named, and its playlists looked up.
    Known(SceneView),
    /// Not connected, or no program scene read yet.
    Unknown,
    /// The scene's playlist lookup failed (#218): its playlists are not known.
    LookupFailed,
}

/// Read a snapshot's program scene: known only when the OBS client is
/// connected, named cg OBS's program scene, and looked up its playlists. Not
/// connected is always unknown (the reconnect is then followed again), even
/// if a failed lookup were still flagged.
fn program_scene(snapshot: &ObsSnapshot) -> ProgramScene {
    match (&snapshot.current_scene, snapshot.connected) {
        (_, false) => ProgramScene::Unknown,
        _ if snapshot.lookup_failed.is_some() => ProgramScene::LookupFailed,
        (Some(scene), true) => ProgramScene::Known(SceneView {
            scene: scene.clone(),
            playlists: snapshot.active_playlist_ids.clone(),
        }),
        _ => ProgramScene::Unknown,
    }
}

/// The follow task's state between two snapshots.
struct FollowLoop {
    follow: Follow,
    settings: FollowSettings,
    /// The program scene of the last snapshot that knew one; `None` once a
    /// snapshot did not (a disconnect) or a catch-up is still to be followed.
    /// A snapshot acts only when its scene differs from this one.
    seen: Option<SceneView>,
}

impl FollowLoop {
    fn new(follow: Follow, settings: FollowSettings) -> Self {
        Self {
            follow,
            settings,
            seen: None,
        }
    }

    /// Keep `snapshot`'s transition as cg OBS's (the last known one stays
    /// while it has none) and put the spec on the bus.
    fn take_transition(&self, snapshot: &ObsSnapshot) {
        if let Some(transition) = &snapshot.transition {
            self.follow
                .bus
                .follow()
                .set_obs_transition(transition.clone());
        }
        self.follow.apply_spec(&self.settings);
    }

    /// One snapshot of the OBS client: take its transition (the spec is
    /// applied BEFORE any cut), then follow its program scene when it changed.
    /// A `catch_up` (the start, the follow switched on) forgets the seen
    /// scene first, so the scene is followed whether it changed or not — this
    /// one, or the next known one when this one is not. A lookup-failed
    /// snapshot is ignored (`seen` is kept).
    async fn on_snapshot(&mut self, snapshot: &ObsSnapshot, catch_up: bool) {
        self.take_transition(snapshot);
        if catch_up {
            self.seen = None;
        }
        match program_scene(snapshot) {
            ProgramScene::Known(view) => {
                if self.seen.as_ref() != Some(&view) && self.settings.follow_obs {
                    self.follow.follow_scene(&view.scene, &view.playlists).await;
                }
                self.seen = Some(view);
            }
            ProgramScene::Unknown => self.seen = None,
            ProgramScene::LookupFailed => debug!(
                scene = ?snapshot.lookup_failed,
                "program follow: the OBS client's scene lookup failed — waiting for its repair"
            ),
        }
    }

    /// The settings poll: re-read the settings, apply the spec, then catch up
    /// to cg OBS's current program scene when the follow was just switched on.
    async fn on_tick(&mut self, obs: &watch::Receiver<ObsSnapshot>) {
        let was_following = self.settings.follow_obs;
        self.settings = self.follow.load(self.settings).await;
        self.follow.apply_spec(&self.settings);
        if self.settings.follow_obs && !was_following {
            let snapshot = obs.borrow().clone();
            self.on_snapshot(&snapshot, true).await;
        }
    }
}

/// Keep the bus's transition spec in step with the settings and cg OBS, and
/// follow cg OBS's program scene while `program_follow_obs` is on, until
/// shutdown. `obs` = the OBS client's snapshots; once that channel is closed
/// (no OBS client) only the settings polls run.
pub async fn run_follow_task(
    follow: Follow,
    mut obs: watch::Receiver<ObsSnapshot>,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    let settings = follow.load(FollowSettings::default()).await;
    let mut task = FollowLoop::new(follow, settings);
    let snapshot = obs.borrow_and_update().clone();
    task.on_snapshot(&snapshot, true).await;
    let mut obs_open = true;
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            changed = obs.changed(), if obs_open => match changed {
                Ok(()) => {
                    let snapshot = obs.borrow_and_update().clone();
                    task.on_snapshot(&snapshot, false).await;
                }
                Err(_) => {
                    obs_open = false;
                    debug!("program follow: no OBS client — only the settings polls run");
                }
            },
            _ = tick.tick() => task.on_tick(&obs).await,
        }
    }
    info!("program follow: task stopped");
}

#[cfg(test)]
#[path = "program_follow_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "program_follow_tests_loop.rs"]
mod tests_loop;
#[cfg(test)]
#[path = "program_follow_tests_task.rs"]
mod tests_task;
