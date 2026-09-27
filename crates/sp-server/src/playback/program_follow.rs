//! `SP-program` follows cg OBS natively, and the transition every program cut
//! uses (#215, B5 of EPIC #174). Design record: #215 comment 5853036223.
//!
//! - **Transition spec.** The task keeps the program bus's spec
//!   (`ProgramBus::set_transition`) in step with the operator's settings and cg
//!   OBS's current scene transition. `program_transition` = `obs` (the default)
//!   uses cg OBS's `GetCurrentSceneTransition`: `fade_transition` → a Fade of
//!   its duration, `cut_transition` → Cut, any other kind → a Fade of its
//!   duration. `fade` / `cut` override it, and `program_transition_ms` is the
//!   fade length when SongPlayer picks it (`program_transition::effective_spec`).
//!   Every cut (dashboard, #213 remote control, this follow) uses that spec.
//! - **cg OBS's transition** is asked through SongPlayer's existing OBS client
//!   (`remote::Upstream`, never a second connection): once at start, on every
//!   (re)connect, on `CurrentSceneTransitionChanged` /
//!   `CurrentSceneTransitionDurationChanged` (the OBS client subscribes to the
//!   Transitions events for this) and after missed events. A read that got no
//!   answer (cg OBS answers `Connected`'s read only once its connection loop
//!   runs) is asked again on the settings polls until one is answered, but
//!   only while cg OBS is up (never without an OBS client). The last answer
//!   is kept while cg OBS is away.
//! - **Follow** (`program_follow_obs`, off by default). On every program scene
//!   change of cg OBS (`ObsEvent::SceneChanged`, SongPlayer's own derived form
//!   of `CurrentProgramSceneChanged`: its playlists come from `check_scene_items`
//!   over the same `NdiSourceMap` the #213 facade's lookup uses, and the #170
//!   poll reconciles an event cg OBS itself dropped), the scene is mapped with
//!   `remote::map::scene_action`, the #213 rule. Exactly one playlist → that
//!   playlist; otherwise "OBS manuál" while it is a source; else keep. The
//!   cut then goes through `program_bus::persist_and_cut`. A scene that maps to
//!   the on-program source cuts nothing. The follow also CATCHES UP to cg OBS's
//!   current program scene (`GetCurrentProgramScene` + the scene's playlists)
//!   at start, when it is switched on, and after events this task missed (a
//!   lagged broadcast), while cg OBS is up; the scene changes queued before
//!   that read are dropped, and the newest of them is followed when cg OBS
//!   does not name its scene (or names that scene without its playlists).
//!   This replaces the event-night watcher script
//!   `%TEMP%\sp_follow.ps1`, which polled the scene.
//! - The settings are re-read every [`FOLLOW_SETTINGS_POLL`], so a save
//!   applies within 5 s. The telemetry ([`FollowShared`], on
//!   `ProgramBus::follow()`) is served as `follow` on `GET /api/v1/program`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sp_core::config::{
    SETTING_PROGRAM_FOLLOW_OBS, SETTING_PROGRAM_TRANSITION, SETTING_PROGRAM_TRANSITION_MS,
    program_transition_ms,
};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tracing::{debug, info, warn};

use crate::obs::ObsEvent;
use crate::playback::program_bus::{ProgramBus, persist_and_cut};
use crate::playback::program_transition::{
    ObsTransition, TransitionMode, TransitionSpec, effective_spec,
};
use crate::remote::map::{SceneAction, scene_action};
use crate::remote::{RemoteCut, Upstream, clip};

/// How often the task re-reads the settings.
pub const FOLLOW_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The obs-websocket request that reads cg OBS's current scene transition.
pub const GET_CURRENT_SCENE_TRANSITION: &str = "GetCurrentSceneTransition";

/// The obs-websocket request that reads cg OBS's current program scene.
pub const GET_CURRENT_PROGRAM_SCENE: &str = "GetCurrentProgramScene";

/// The most transition re-reads one catch-up makes before its scene read; a
/// transition still changing after them is read again by the settings polls.
const MAX_CATCH_UP_REREADS: usize = 3;

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

/// cg OBS's transition from the op=7 `d` object of `GetCurrentSceneTransition`;
/// `None` when the request failed or carries no kind.
pub fn obs_transition_from_reply(d: &Value) -> Option<ObsTransition> {
    if d["requestStatus"]["result"].as_bool() != Some(true) {
        return None;
    }
    let data = &d["responseData"];
    Some(ObsTransition {
        name: clip(data["transitionName"].as_str().unwrap_or_default()),
        kind: clip(data["transitionKind"].as_str()?),
        duration_ms: data["transitionDuration"]
            .as_u64()
            .map(|ms| u32::try_from(ms).unwrap_or(u32::MAX)),
    })
}

/// cg OBS's program scene from the op=7 `d` object of `GetCurrentProgramScene`;
/// `None` when the request failed or names no scene.
pub fn program_scene_from_reply(d: &Value) -> Option<String> {
    if d["requestStatus"]["result"].as_bool() != Some(true) {
        return None;
    }
    d["responseData"]["currentProgramSceneName"]
        .as_str()
        .map(str::to_string)
}

/// A cg OBS event that changes its current transition (kind or duration).
pub fn is_transition_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "CurrentSceneTransitionChanged" | "CurrentSceneTransitionDurationChanged"
    )
}

/// Whether `event` asks for cg OBS's transition to be read again: a
/// (re)connect, or a change of cg OBS's current transition.
fn rereads_transition(event: &ObsEvent) -> bool {
    match event {
        ObsEvent::Connected => true,
        ObsEvent::Raw { event_type, .. } => is_transition_event(event_type),
        ObsEvent::SceneChanged { .. } | ObsEvent::Disconnected => false,
    }
}

/// `GET /api/v1/program` → `follow`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FollowStatus {
    /// From the STORED settings (a save shows at once).
    pub enabled: bool,
    pub mode: TransitionMode,
    pub ms: u32,
    /// cg OBS's current scene transition, `None` until cg OBS answered.
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

    /// Ask cg OBS for its current transition and keep it; `true` when cg OBS
    /// answered. No answer keeps the last known one (the task asks again on
    /// its settings polls while cg OBS is up). `retry`: such a poll's retry,
    /// logged at debug so a failure streak WARNs once.
    pub async fn refresh_obs(&self, upstream: &Upstream, retry: bool) -> bool {
        let reply = upstream.request(GET_CURRENT_SCENE_TRANSITION, None).await;
        let Some(obs) = reply.as_ref().and_then(obs_transition_from_reply) else {
            if retry {
                debug!("program follow: cg OBS still did not report its scene transition");
            } else {
                warn!(
                    "program follow: cg OBS did not report its scene transition — keeping the last known one"
                );
            }
            return false;
        };
        if self.bus.follow().set_obs_transition(obs.clone()) {
            info!(
                name = %obs.name,
                kind = %obs.kind,
                duration_ms = ?obs.duration_ms,
                "program follow: cg OBS's scene transition"
            );
        }
        true
    }

    /// Follow cg OBS's CURRENT program scene, read through the OBS client: at
    /// start, when the follow is switched on (the next scene change may be
    /// long away), and after missed events (a lost `SceneChanged`). This is
    /// what the retired watcher script got by polling.
    ///
    /// `dropped` is the newest scene change the caller dropped unhandled. It is
    /// followed instead when cg OBS does not name its program scene, or names
    /// that same scene but not its playlists: the dropped event may be the only
    /// news of the change, because the #170 poll does not repeat a scene the
    /// OBS client already stored. A dropped change of ANOTHER scene is older
    /// than the one cg OBS named, so it is never followed. Ungated: the task
    /// calls it only through `FollowLoop::catch_up`, which checks cg OBS is up.
    pub async fn follow_current_scene(
        &self,
        upstream: &Upstream,
        dropped: Option<(String, HashSet<i64>)>,
    ) {
        let reply = upstream.request(GET_CURRENT_PROGRAM_SCENE, None).await;
        let Some(scene) = reply.as_ref().and_then(program_scene_from_reply) else {
            match dropped {
                Some((named, playlists)) => {
                    info!(
                        scene = %clip(&named),
                        "program follow: cg OBS did not report its program scene — following the newest scene change it sent"
                    );
                    self.follow_scene(&named, &playlists).await;
                }
                None => {
                    warn!("program follow: cg OBS did not report its program scene — not caught up")
                }
            }
            return;
        };
        let Some(playlists) = upstream.scene_playlists(&scene).await else {
            match dropped {
                Some((named, playlists)) if named == scene => {
                    info!(
                        scene = %clip(&scene),
                        "program follow: cg OBS did not report the scene's playlists — following them from its scene change"
                    );
                    self.follow_scene(&named, &playlists).await;
                }
                _ => warn!(
                    scene = %clip(&scene),
                    "program follow: cg OBS did not report the scene's playlists — not caught up"
                ),
            }
            return;
        };
        self.follow_scene(&scene, &playlists).await;
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

/// Start the task (called once from `PlaybackEngine::start_program`).
#[cfg_attr(test, mutants::skip)] // orchestration glue; the task itself is tested
pub fn start_follow(follow: Follow, upstream: Upstream, shutdown: &broadcast::Sender<()>) {
    let rx = shutdown.subscribe();
    tokio::spawn(run_follow_task(follow, upstream, rx, FOLLOW_SETTINGS_POLL));
}

/// The follow task's state between two cg OBS events.
struct FollowLoop {
    follow: Follow,
    upstream: Upstream,
    settings: FollowSettings,
    /// cg OBS's connection as the OBS client reports it. Never up without an
    /// OBS client; with one it is assumed up until the client says otherwise,
    /// so the start's reads count. Every event but `Disconnected` comes from
    /// a live connection, so each one says it is up: a `Connected` lost in a
    /// lagged broadcast must not leave it down.
    obs_up: bool,
    /// The last transition read got no answer. It is asked again on every
    /// settings poll, but only while cg OBS is up: a call made while cg OBS
    /// is away waits in the OBS client's command queue (served only while
    /// connected, 64 deep), and a full queue blocks that client's other
    /// senders.
    read_pending: bool,
    /// The newest scene change a drain dropped, until a `Connected` (the new
    /// connection re-reports its scene) or a lag (a newer one may be among the
    /// lost events). The catch-up after the drain hands it to
    /// `Follow::follow_current_scene`. An unanswered catch-up with nothing
    /// dropped is not retried: the program then waits for cg OBS's next scene
    /// change (or reconnect).
    missed_scene: Option<(String, HashSet<i64>)>,
}

impl FollowLoop {
    fn new(follow: Follow, upstream: Upstream, settings: FollowSettings) -> Self {
        let obs_up = upstream.is_configured();
        Self {
            follow,
            upstream,
            settings,
            obs_up,
            read_pending: false,
            missed_scene: None,
        }
    }

    /// Read cg OBS's transition (`retry`: a settings poll's retry).
    async fn read_transition(&mut self, retry: bool) {
        self.read_pending = !self.follow.refresh_obs(&self.upstream, retry).await;
    }

    /// cg OBS's connection after `event` (see `obs_up`).
    fn note_connection(&mut self, event: &ObsEvent) {
        self.obs_up = !matches!(event, ObsEvent::Disconnected);
    }

    /// One cg OBS event.
    async fn on_event(&mut self, event: ObsEvent) {
        self.note_connection(&event);
        if rereads_transition(&event) {
            self.read_transition(false).await;
        } else if let ObsEvent::SceneChanged {
            scene_name,
            active_playlist_ids,
        } = event
            && self.settings.follow_obs
        {
            self.follow
                .follow_scene(&scene_name, &active_playlist_ids)
                .await;
        }
    }

    /// Drop every event still queued: each one is older than the read that
    /// follows. A scene change would cut back to a scene cg OBS already left;
    /// the newest one is kept as `missed_scene`. Keeps cg OBS's connection
    /// state, and returns whether cg OBS's transition must be read again (an
    /// event asked for it, or events were lost to a lag).
    fn drain(&mut self, events: &mut broadcast::Receiver<ObsEvent>) -> bool {
        let mut reread = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    self.note_connection(&event);
                    reread |= rereads_transition(&event);
                    match event {
                        ObsEvent::SceneChanged {
                            scene_name,
                            active_playlist_ids,
                        } => self.missed_scene = Some((scene_name, active_playlist_ids)),
                        ObsEvent::Connected => self.missed_scene = None,
                        ObsEvent::Raw { .. } | ObsEvent::Disconnected => {}
                    }
                }
                Err(TryRecvError::Lagged(_)) => {
                    self.missed_scene = None;
                    reread = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => return reread,
            }
        }
    }

    /// At start and after missed events: drop the queued events, re-read cg
    /// OBS's transition while it is up, put the spec on the bus, then catch
    /// up.
    async fn resync(&mut self, events: &mut broadcast::Receiver<ObsEvent>) {
        self.drain(events);
        if self.obs_up {
            self.read_transition(false).await;
        }
        self.follow.apply_spec(&self.settings);
        self.catch_up(events).await;
    }

    /// Drop the queued events (always, following or not); when one of them
    /// changed cg OBS's transition, read and apply it again and drop what
    /// queued during THAT read too, so a drain always comes right before the
    /// scene read (after [`MAX_CATCH_UP_REREADS`] re-reads the polls take
    /// over). Then, while following and cg OBS is up, cut to cg OBS's current
    /// program scene with the spec on the bus (`Follow::follow_current_scene`,
    /// handed the newest dropped scene change).
    async fn catch_up(&mut self, events: &mut broadcast::Receiver<ObsEvent>) {
        let mut reread = self.drain(events);
        for _ in 0..MAX_CATCH_UP_REREADS {
            if !(reread && self.obs_up) {
                break;
            }
            self.read_transition(false).await;
            self.follow.apply_spec(&self.settings);
            reread = self.drain(events);
        }
        self.read_pending |= reread;
        let dropped = self.missed_scene.take();
        if self.obs_up && self.settings.follow_obs {
            self.follow
                .follow_current_scene(&self.upstream, dropped)
                .await;
        }
    }

    /// The settings poll: re-read the settings, retry an unanswered transition
    /// read while cg OBS is up, apply the spec, then catch up when the follow
    /// was just switched on (with the spec just applied). Switched on while
    /// cg OBS is away, the follow catches up on the reconnect: the OBS client
    /// then reports cg OBS's program scene as a `SceneChanged`.
    async fn on_tick(&mut self, events: &mut broadcast::Receiver<ObsEvent>) {
        let was_following = self.settings.follow_obs;
        self.settings = self.follow.load(self.settings).await;
        if self.read_pending && self.obs_up {
            self.read_transition(true).await;
        }
        self.follow.apply_spec(&self.settings);
        if self.settings.follow_obs && !was_following {
            self.catch_up(events).await;
        }
    }
}

/// Keep the bus's transition spec in step with the settings and cg OBS, and
/// follow cg OBS's program scene while `program_follow_obs` is on, until
/// shutdown.
pub async fn run_follow_task(
    follow: Follow,
    upstream: Upstream,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    let mut events = upstream.subscribe();
    let settings = follow.load(FollowSettings::default()).await;
    let mut task = FollowLoop::new(follow, upstream, settings);
    // cg OBS's transition first, so the very first spec already uses it.
    task.resync(&mut events).await;
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            event = events.recv() => match event {
                Ok(event) => task.on_event(event).await,
                Err(RecvError::Lagged(n)) => {
                    warn!(
                        n,
                        "program follow: missed cg OBS events — re-reading its transition and program scene"
                    );
                    task.resync(&mut events).await;
                }
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick() => task.on_tick(&mut events).await,
        }
        task.follow.apply_spec(&task.settings);
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
