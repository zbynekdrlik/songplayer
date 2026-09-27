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
//!   runs) is asked again on every settings poll until one is answered. The
//!   last answer is kept while cg OBS is away.
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
//!   lagged broadcast). This replaces the event-night watcher script
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
};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::obs::ObsEvent;
use crate::playback::program_bus::{ProgramBus, persist_and_cut};
use crate::playback::program_transition::{
    ObsTransition, TransitionMode, TransitionSpec, effective_spec, parse_transition_ms,
};
use crate::remote::map::{SceneAction, scene_action};
use crate::remote::{RemoteCut, Upstream, clip};

/// How often the task re-reads the settings.
pub const FOLLOW_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// The obs-websocket request that reads cg OBS's current scene transition.
pub const GET_CURRENT_SCENE_TRANSITION: &str = "GetCurrentSceneTransition";

/// The obs-websocket request that reads cg OBS's current program scene.
pub const GET_CURRENT_PROGRAM_SCENE: &str = "GetCurrentProgramScene";

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
    let ms = parse_transition_ms(
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
    /// its next settings poll).
    pub async fn refresh_obs(&self, upstream: &Upstream) -> bool {
        let reply = upstream.request(GET_CURRENT_SCENE_TRANSITION, None).await;
        let Some(obs) = reply.as_ref().and_then(obs_transition_from_reply) else {
            warn!(
                "program follow: cg OBS did not report its scene transition — keeping the last known one"
            );
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
    pub async fn catch_up(&self, upstream: &Upstream) {
        let reply = upstream.request(GET_CURRENT_PROGRAM_SCENE, None).await;
        let Some(scene) = reply.as_ref().and_then(program_scene_from_reply) else {
            warn!("program follow: cg OBS did not report its program scene — not caught up");
            return;
        };
        let Some(playlists) = upstream.scene_playlists(&scene).await else {
            warn!(
                scene = %clip(&scene),
                "program follow: cg OBS did not report the scene's playlists — not caught up"
            );
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
    let mut settings = follow.load(FollowSettings::default()).await;
    // cg OBS's transition first, so the very first spec already uses it. A
    // read that got no answer (cg OBS away or still starting its connection)
    // is asked again on every settings poll until one is answered.
    let mut obs_known = follow.refresh_obs(&upstream).await;
    follow.apply_spec(&settings);
    if settings.follow_obs {
        follow.catch_up(&upstream).await;
    }
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            event = events.recv() => match event {
                Ok(ObsEvent::SceneChanged { scene_name, active_playlist_ids }) => {
                    if settings.follow_obs {
                        follow.follow_scene(&scene_name, &active_playlist_ids).await;
                    }
                }
                Ok(ObsEvent::Connected) => obs_known = follow.refresh_obs(&upstream).await,
                Ok(ObsEvent::Raw { event_type, .. }) if is_transition_event(&event_type) => {
                    obs_known = follow.refresh_obs(&upstream).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    warn!(
                        n,
                        "program follow: missed cg OBS events — re-reading its transition and program scene"
                    );
                    obs_known = follow.refresh_obs(&upstream).await;
                    if settings.follow_obs {
                        follow.catch_up(&upstream).await;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                let was_following = settings.follow_obs;
                settings = follow.load(settings).await;
                if settings.follow_obs && !was_following {
                    follow.catch_up(&upstream).await;
                }
                if !obs_known {
                    obs_known = follow.refresh_obs(&upstream).await;
                }
            }
        }
        follow.apply_spec(&settings);
    }
    info!("program follow: task stopped");
}

#[cfg(test)]
#[path = "program_follow_tests.rs"]
mod tests;
