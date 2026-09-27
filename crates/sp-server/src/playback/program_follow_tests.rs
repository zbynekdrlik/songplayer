//! #215: `SP-program` follows cg OBS and keeps the transition spec in step.
//! The reply parsing, the settings, the telemetry and `follow_scene` over a
//! real pool + `ProgramBus`, then the task end to end against a fake cg OBS at
//! the OBS client's command channel (`ObsCommand::Remote`), driven by events
//! on the client's broadcast. Every wait is bounded (20 s); an event's effect
//! is proven by a later event whose own effect is observable, never by a sleep.
//! Wired via `#[cfg(test)] #[path = "program_follow_tests.rs"] mod tests;`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::*;
use crate::obs::remote_call::RemoteCall;
use crate::obs::{ObsCommand, ObsEvent};
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE};
use crate::playback::program_transition::{
    ObsTransition, SpecSource, TransitionKind, TransitionMode, TransitionSpec,
};
use crate::remote::map::{KeepReason, SceneAction};
use crate::remote::{RemoteCut, Upstream};

const REQUEST: &str = "GetCurrentSceneTransition";

/// The op=7 `d` object cg OBS answers `GetCurrentSceneTransition` with.
fn reply(name: &str, kind: &str, duration: Value) -> Value {
    json!({
        "requestType": REQUEST,
        "requestId": "sp-1",
        "requestStatus": { "result": true, "code": 100 },
        "responseData": {
            "transitionName": name,
            "transitionUuid": "uuid-1",
            "transitionKind": kind,
            "transitionFixed": duration.is_null(),
            "transitionDuration": duration,
            "transitionConfigurable": true,
            "transitionSettings": {},
        },
    })
}

fn fade(ms: u64) -> Value {
    reply("Fade", "fade_transition", json!(ms))
}

fn obs(name: &str, kind: &str, duration_ms: Option<u32>) -> ObsTransition {
    ObsTransition {
        name: name.to_string(),
        kind: kind.to_string(),
        duration_ms,
    }
}

fn set(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

/// The last scene change the follow handled (`follow.last_follow_cut`).
fn last_cut(bus: &ProgramBus) -> RemoteCut {
    bus.follow()
        .status(&FollowSettings::default())
        .last_follow_cut
        .expect("the follow is recorded")
}

/// The spec on the bus as `(kind, duration_ms, n_slots, source)`.
fn spec_of(bus: &ProgramBus) -> (TransitionKind, u32, u32, SpecSource) {
    let t = bus.status().transition;
    (t.kind, t.duration_ms, t.n_slots, t.source)
}

#[test]
fn cg_obs_transition_is_read_from_its_reply() {
    assert_eq!(
        obs_transition_from_reply(&fade(300)),
        Some(obs("Fade", "fade_transition", Some(300)))
    );
    assert_eq!(
        obs_transition_from_reply(&reply("Cut", "cut_transition", Value::Null)),
        Some(obs("Cut", "cut_transition", None)),
        "a fixed transition has no duration"
    );
    assert_eq!(
        obs_transition_from_reply(&fade(5_000_000_000)).and_then(|t| t.duration_ms),
        Some(u32::MAX),
        "an absurd duration saturates"
    );
    let failed = json!({
        "requestType": REQUEST,
        "requestStatus": { "result": false, "code": 600, "comment": "nope" },
    });
    assert_eq!(obs_transition_from_reply(&failed), None);
    assert_eq!(obs_transition_from_reply(&json!({})), None, "no status");
    let no_kind = json!({
        "requestStatus": { "result": true, "code": 100 },
        "responseData": { "transitionName": "Fade", "transitionDuration": 300 },
    });
    assert_eq!(obs_transition_from_reply(&no_kind), None, "no kind");
    let long_name = "x".repeat(100);
    let t = obs_transition_from_reply(&reply(&long_name, "fade_transition", json!(300)))
        .expect("a transition");
    assert_eq!(t.name.chars().count(), 64, "an OBS-chosen name is clipped");
    let unnamed = json!({
        "requestStatus": { "result": true, "code": 100 },
        "responseData": { "transitionKind": "swipe_transition" },
    });
    assert_eq!(
        obs_transition_from_reply(&unnamed),
        Some(obs("", "swipe_transition", None))
    );
    assert_eq!(GET_CURRENT_SCENE_TRANSITION, REQUEST);
}

#[test]
fn only_the_two_transition_events_trigger_a_reread() {
    assert!(is_transition_event("CurrentSceneTransitionChanged"));
    assert!(is_transition_event("CurrentSceneTransitionDurationChanged"));
    assert!(!is_transition_event("SceneTransitionStarted"));
    assert!(!is_transition_event("CurrentProgramSceneChanged"));
    assert!(!is_transition_event(""));
    assert_eq!(FOLLOW_SETTINGS_POLL, Duration::from_secs(5));
}

#[tokio::test]
async fn the_follow_settings_default_off_obs_300_and_load_trimmed() {
    let pool = pool().await;
    assert_eq!(
        FollowSettings::default(),
        FollowSettings {
            follow_obs: false,
            mode: TransitionMode::Obs,
            ms: 300,
        }
    );
    assert_eq!(
        load_follow_settings(&pool).await.unwrap(),
        FollowSettings::default()
    );
    store(&pool, "program_follow_obs", " true ").await;
    store(&pool, "program_transition", " fade ").await;
    store(&pool, "program_transition_ms", "450").await;
    let loaded = load_follow_settings(&pool).await.unwrap();
    assert_eq!(
        loaded,
        FollowSettings {
            follow_obs: true,
            mode: TransitionMode::Fade,
            ms: 450,
        }
    );
    store(&pool, "program_follow_obs", "True").await;
    assert!(
        !load_follow_settings(&pool).await.unwrap().follow_obs,
        "only \"true\" follows"
    );
    // An unreadable store keeps the settings in force.
    let follow = Follow::new(pool.clone(), Arc::new(ProgramBus::new()));
    pool.close().await;
    assert!(load_follow_settings(&pool).await.is_err());
    assert_eq!(follow.load(loaded).await, loaded);
}

#[test]
fn the_follow_telemetry_reports_the_stored_settings_and_what_it_saw() {
    let shared = FollowShared::default();
    assert_eq!(shared.obs_transition(), None);
    assert!(shared.set_obs_transition(obs("Fade", "fade_transition", Some(300))));
    assert!(
        !shared.set_obs_transition(obs("Fade", "fade_transition", Some(300))),
        "the same transition again is no change"
    );
    assert!(shared.set_obs_transition(obs("Fade", "fade_transition", Some(500))));
    let settings = FollowSettings {
        follow_obs: true,
        mode: TransitionMode::Cut,
        ms: 700,
    };
    assert_eq!(
        shared.status(&settings),
        FollowStatus {
            enabled: true,
            mode: TransitionMode::Cut,
            ms: 700,
            obs_transition: Some(obs("Fade", "fade_transition", Some(500))),
            last_follow_cut: None,
        }
    );
}

#[tokio::test]
async fn apply_spec_puts_the_override_or_cg_obs_transition_on_the_bus() {
    let bus = Arc::new(ProgramBus::new());
    let follow = Follow::new(pool().await, bus.clone());
    let obs_mode = FollowSettings::default();
    assert_eq!(
        follow.apply_spec(&obs_mode),
        TransitionSpec::fade(300, SpecSource::Fallback),
        "cg OBS's transition is not known yet: a fade of the setting's length"
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback)
    );
    bus.follow()
        .set_obs_transition(obs("Cut", "cut_transition", None));
    follow.apply_spec(&obs_mode);
    assert_eq!(spec_of(&bus), (TransitionKind::Cut, 0, 0, SpecSource::Obs));
    let fade_mode = FollowSettings {
        mode: TransitionMode::Fade,
        ms: 1000,
        ..obs_mode
    };
    follow.apply_spec(&fade_mode);
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 1000, 30, SpecSource::Setting),
        "the override wins over cg OBS"
    );
}

#[tokio::test]
async fn a_scene_showing_one_playlist_cuts_the_program_to_it_and_persists_it() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(3);
    let follow = Follow::new(pool.clone(), bus.clone());
    assert_eq!(
        follow.follow_scene("sp-fast", &set(&[7])).await,
        SceneAction::Playlist(7)
    );
    let st = bus.status();
    assert_eq!((st.source, st.previous), (Some(7), Some(3)));
    assert_eq!(st.health.cuts, 1);
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );
    let cut = last_cut(&bus);
    assert_eq!(
        (cut.scene.as_str(), cut.action, cut.source, cut.reason),
        ("sp-fast", "playlist", Some(7), None)
    );
    assert_eq!(cut.cut_boundary_100ns, st.cut_boundary_100ns);
    assert!(cut.cut_boundary_100ns.is_some());
    assert!(cut.at_ms > 0);
}

#[tokio::test]
async fn a_manual_scene_cuts_to_obs_manual_only_while_the_input_is_a_source() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(3);
    let follow = Follow::new(pool.clone(), bus.clone());
    // The input is off: a manual (or multi-playlist) scene keeps the program.
    assert_eq!(
        follow.follow_scene("Slido", &set(&[3, 7])).await,
        SceneAction::Keep(KeepReason::InputInactive)
    );
    assert_eq!(bus.status().source, Some(3));
    assert_eq!(bus.status().health.cuts, 0);
    let kept = last_cut(&bus);
    assert_eq!(
        (
            kept.action,
            kept.source,
            kept.reason,
            kept.cut_boundary_100ns
        ),
        ("keep", None, Some("input_inactive"), None)
    );
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None,
        "nothing persisted"
    );
    // The input is a source: the same scene cuts to "OBS manuál" (-1).
    store(&pool, "ndi_input_enabled", "true").await;
    store(&pool, "ndi_input_source", "CG-OBS (manual)").await;
    assert_eq!(
        follow.follow_scene("Slido", &set(&[])).await,
        SceneAction::Input
    );
    assert_eq!(bus.status().source, Some(-1));
    let input = last_cut(&bus);
    assert_eq!(
        (input.action, input.source, input.reason),
        ("input", Some(-1), None)
    );
}

#[tokio::test]
async fn a_scene_already_on_program_cuts_nothing_and_a_failed_persist_cuts_nothing() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7);
    let follow = Follow::new(pool.clone(), bus.clone());
    assert_eq!(
        follow.follow_scene("sp-fast", &set(&[7])).await,
        SceneAction::Playlist(7)
    );
    assert_eq!(bus.status().health.cuts, 0, "it already shows 7");
    assert_eq!(
        crate::db::models::get_setting(&pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap(),
        None,
        "nothing persisted"
    );
    let same = last_cut(&bus);
    assert_eq!(
        (same.source, same.reason, same.cut_boundary_100ns),
        (Some(7), None, None)
    );

    // The store is gone: the source cannot be persisted, so nothing is cut.
    pool.close().await;
    assert_eq!(
        follow.follow_scene("sp-slow", &set(&[8])).await,
        SceneAction::Playlist(8)
    );
    assert_eq!(bus.status().source, Some(7));
    assert_eq!(bus.status().health.cuts, 0);
    let failed = last_cut(&bus);
    assert_eq!(
        (failed.scene.as_str(), failed.source, failed.reason),
        ("sp-slow", Some(8), Some("persist_failed"))
    );
}

// ---- the task, against a fake cg OBS ----------------------------------------

/// The obs-websocket request that reads cg OBS's current program scene.
const PROGRAM_SCENE: &str = "GetCurrentProgramScene";

/// The playlists each of the fake cg OBS's scenes shows: `sp-fast` → 7,
/// `sp-slow` → 8, any other scene is a manual one (none).
fn playlists_of(scene: &str) -> HashSet<i64> {
    match scene {
        "sp-fast" => set(&[7]),
        "sp-slow" => set(&[8]),
        _ => set(&[]),
    }
}

/// The follow task running against a fake cg OBS.
struct TaskRig {
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    /// cg OBS's events, as the OBS client broadcasts them.
    events: broadcast::Sender<ObsEvent>,
    /// Every call the fake cg OBS answered, in order: a request by its type,
    /// a scene → playlists lookup as `ScenePlaylists:<scene>`.
    seen: mpsc::UnboundedReceiver<String>,
    /// The fake cg OBS's program scene (initially the manual scene `Slido`).
    program_scene: Arc<std::sync::Mutex<String>>,
    shutdown: broadcast::Sender<()>,
    task: JoinHandle<()>,
}

/// Start the task over `pool` (settings stored first) against a fake cg OBS
/// at the OBS client's command channel. It answers the n-th
/// `GetCurrentSceneTransition` with `replies[n]` (the last one repeats),
/// `GetCurrentProgramScene` with `program_scene`, and a scene lookup with
/// [`playlists_of`]. The event broadcast holds `capacity` events; the
/// settings are re-read every `poll`.
fn start(pool: &SqlitePool, replies: Vec<Value>, capacity: usize, poll: Duration) -> TaskRig {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(16);
    let (events, _) = broadcast::channel::<ObsEvent>(capacity);
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let program_scene = Arc::new(std::sync::Mutex::new("Slido".to_string()));
    let current = program_scene.clone();
    tokio::spawn(async move {
        let mut n: usize = 0;
        while let Some(ObsCommand::Remote(call)) = cmd_rx.recv().await {
            match call {
                RemoteCall::Request {
                    request_type,
                    reply,
                    ..
                } => {
                    let d = if request_type == PROGRAM_SCENE {
                        let scene = current.lock().unwrap().clone();
                        json!({
                            "requestType": PROGRAM_SCENE,
                            "requestStatus": { "result": true, "code": 100 },
                            "responseData": {
                                "currentProgramSceneName": scene,
                                "currentProgramSceneUuid": "uuid-scene",
                            },
                        })
                    } else {
                        n += 1;
                        replies[(n - 1).min(replies.len() - 1)].clone()
                    };
                    let _ = seen_tx.send(request_type);
                    let _ = reply.send(Some(d));
                }
                RemoteCall::ScenePlaylists { scene, reply } => {
                    let _ = seen_tx.send(format!("ScenePlaylists:{scene}"));
                    let _ = reply.send(playlists_of(&scene));
                }
            }
        }
    });
    let bus = Arc::new(ProgramBus::new());
    let follow = Follow::new(pool.clone(), bus.clone());
    let upstream = Upstream::new(Some(cmd_tx), events.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let task = tokio::spawn(run_follow_task(follow, upstream, rx, poll));
    TaskRig {
        pool: pool.clone(),
        bus,
        events,
        seen,
        program_scene,
        shutdown,
        task,
    }
}

/// A poll interval that never comes round again after the first tick.
const NO_POLL: Duration = Duration::from_secs(3600);

impl TaskRig {
    /// The next request the fake cg OBS answered, within 20 s.
    async fn next_request(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(20), self.seen.recv())
            .await
            .expect("a request to cg OBS within 20 s")
            .expect("the fake cg OBS is running")
    }

    /// The next `n` calls the fake cg OBS answered, each within 20 s.
    async fn next_requests(&mut self, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(self.next_request().await);
        }
        out
    }

    /// cg OBS switches its program scene (no event: only a read sees it).
    fn switch_program_scene(&self, scene: &str) {
        *self.program_scene.lock().unwrap() = scene.to_string();
    }

    /// The requests answered since the last look (no wait).
    fn more_requests(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(r) = self.seen.try_recv() {
            out.push(r);
        }
        out
    }

    /// Wait (at most 20 s) until the bus's spec is `want`.
    async fn spec_becomes(&self, want: (TransitionKind, u32, u32, SpecSource)) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while spec_of(&self.bus) != want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the spec never became {want:?}: {:?}",
                spec_of(&self.bus)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait (at most 20 s) until `SP-program` shows `source`.
    async fn source_becomes(&self, source: i64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while self.bus.status().source != Some(source) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the program never cut to {source}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn raw(&self, event_type: &str) {
        self.events
            .send(ObsEvent::Raw {
                event_type: event_type.to_string(),
                event_data: json!({}),
            })
            .expect("the task listens");
    }

    fn scene(&self, name: &str, playlists: &[i64]) {
        self.events
            .send(ObsEvent::SceneChanged {
                scene_name: name.to_string(),
                active_playlist_ids: set(playlists),
            })
            .expect("the task listens");
    }

    /// Stop the task and see it end within 20 s.
    async fn stop(self) {
        self.shutdown.send(()).expect("the task listens");
        tokio::time::timeout(Duration::from_secs(20), self.task)
            .await
            .expect("the task stops on shutdown")
            .expect("the task did not panic");
    }
}

#[tokio::test]
async fn the_task_follows_cg_obs_transition_and_rereads_it_when_it_changes() {
    let pool = pool().await;
    let mut rig = start(
        &pool,
        vec![
            fade(500),
            fade(1000),
            reply("Cut", "cut_transition", Value::Null),
        ],
        16,
        NO_POLL,
    );
    assert_eq!(rig.next_request().await, REQUEST, "asked once at start");
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    rig.raw("CurrentSceneTransitionDurationChanged");
    assert_eq!(rig.next_request().await, REQUEST);
    rig.spec_becomes((TransitionKind::Fade, 1000, 30, SpecSource::Obs))
        .await;
    rig.raw("CurrentSceneTransitionChanged");
    assert_eq!(rig.next_request().await, REQUEST);
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Obs))
        .await;
    assert_eq!(
        rig.bus.follow().obs_transition(),
        Some(obs("Cut", "cut_transition", None))
    );
    rig.stop().await;
}

#[tokio::test]
async fn an_unrelated_event_asks_nothing_a_reconnect_rereads_and_a_scene_is_followed() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut rig = start(&pool, vec![fade(500)], 16, NO_POLL);
    assert_eq!(rig.next_request().await, REQUEST);
    rig.raw("SceneTransitionStarted");
    rig.events
        .send(ObsEvent::Connected)
        .expect("the task listens");
    rig.scene("sp-fast", &[7]);
    // The scene change is handled after the two events before it.
    rig.source_becomes(7).await;
    assert_eq!(
        rig.more_requests(),
        vec![PROGRAM_SCENE, "ScenePlaylists:Slido", REQUEST],
        "the start caught up to cg OBS's manual scene (the input is off: no \
         cut), then only the reconnect asked cg OBS again"
    );
    assert_eq!(
        crate::db::models::get_setting(&rig.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );
    let cut = last_cut(&rig.bus);
    assert_eq!((cut.scene.as_str(), cut.action), ("sp-fast", "playlist"));
    let window = rig.bus.status().transition.active.expect("a fade window");
    assert_eq!(
        (window.from, window.to, window.n_slots),
        (None, 7, 15),
        "the cut uses cg OBS's 500 ms fade"
    );
    rig.stop().await;
}

#[tokio::test]
async fn missed_events_reread_the_transition_and_catch_up_to_the_program_scene() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut rig = start(&pool, vec![fade(500)], 1, NO_POLL);
    assert_eq!(
        rig.next_requests(3).await,
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:Slido"],
        "the transition, then the start's catch-up (a manual scene: no cut)"
    );
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    // cg OBS switched to sp-fast and its scene event is among the missed
    // ones: the broadcast keeps ONE event, so three sent before the task runs
    // again lag it by two, and only the last (a transition change) is left.
    rig.switch_program_scene("sp-fast");
    rig.raw("SceneItemEnableStateChanged");
    rig.raw("InputVolumeChanged");
    rig.raw("CurrentSceneTransitionChanged");
    assert_eq!(
        rig.next_requests(4).await,
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast", REQUEST],
        "the lag re-read the transition and caught up, then the kept event re-read it"
    );
    assert_eq!(
        rig.bus.status().source,
        Some(7),
        "caught up to cg OBS's scene"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_following_task_catches_up_to_cg_obs_program_scene_at_start() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut rig = start(&pool, vec![fade(500)], 16, Duration::from_millis(20));
    rig.switch_program_scene("sp-slow");
    assert_eq!(
        rig.next_requests(3).await,
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-slow"]
    );
    rig.source_becomes(8).await;
    assert_eq!(last_cut(&rig.bus).scene, "sp-slow");
    // While it keeps following, the settings polls never read the scene again.
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    assert_eq!(rig.more_requests(), Vec::<String>::new());
    rig.stop().await;
}

#[tokio::test]
async fn switching_the_follow_on_catches_up_once() {
    let pool = pool().await;
    let mut rig = start(&pool, vec![fade(500)], 16, Duration::from_millis(20));
    rig.switch_program_scene("sp-fast");
    assert_eq!(
        rig.next_request().await,
        REQUEST,
        "no follow: no scene read"
    );
    store(&pool, "program_follow_obs", "true").await;
    rig.source_becomes(7).await;
    assert_eq!(
        rig.more_requests(),
        vec![PROGRAM_SCENE, "ScenePlaylists:sp-fast"],
        "switching it on caught up to cg OBS's scene"
    );
    // Later polls, still following, read nothing.
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    assert_eq!(rig.more_requests(), Vec::<String>::new());
    rig.stop().await;
}

#[tokio::test]
async fn a_failed_transition_read_is_retried_until_cg_obs_answers() {
    let pool = pool().await;
    // cg OBS is still starting: its first answer is a failure.
    let failed = json!({
        "requestType": REQUEST,
        "requestStatus": { "result": false, "code": 207, "comment": "not ready" },
    });
    let mut rig = start(
        &pool,
        vec![failed, fade(500)],
        16,
        Duration::from_millis(20),
    );
    assert_eq!(rig.next_requests(2).await, vec![REQUEST, REQUEST]);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    // Answered: the polls stop asking.
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    assert_eq!(rig.more_requests(), Vec::<String>::new());
    rig.stop().await;
}

#[tokio::test]
async fn with_the_follow_off_a_scene_change_cuts_nothing() {
    let pool = pool().await;
    let mut rig = start(&pool, vec![fade(500)], 16, NO_POLL);
    assert_eq!(rig.next_request().await, REQUEST);
    rig.scene("sp-fast", &[7]);
    // A transition event after it: once cg OBS is asked again, the scene
    // change before it has been handled.
    rig.raw("CurrentSceneTransitionChanged");
    assert_eq!(rig.next_request().await, REQUEST);
    let st = rig.bus.status();
    assert_eq!((st.source, st.health.cuts), (None, 0));
    assert_eq!(
        rig.bus
            .follow()
            .status(&FollowSettings::default())
            .last_follow_cut,
        None
    );
    rig.stop().await;
}

#[tokio::test]
async fn the_settings_are_reread_and_override_cg_obs_transition() {
    let pool = pool().await;
    let mut rig = start(&pool, vec![fade(500)], 16, Duration::from_millis(20));
    assert_eq!(rig.next_request().await, REQUEST);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "fade").await;
    store(&pool, "program_transition_ms", "400").await;
    rig.spec_becomes((TransitionKind::Fade, 400, 12, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "obs").await;
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    rig.stop().await;
}
