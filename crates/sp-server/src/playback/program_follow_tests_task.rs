//! #215: the follow task end to end against a fake cg OBS at the OBS
//! client's command channel (`ObsCommand::Remote`), driven by events on the
//! client's broadcast, plus `FollowLoop`'s own steps (the stale-scene drains,
//! cg OBS's connection state, the retry and the switch-on catch-up gated on
//! cg OBS being up). Every wait is bounded (20 s); an
//! event's effect is proven by a later event whose own effect is observable,
//! never by a sleep. Shares the helpers of `program_follow_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_follow_tests_task.rs"] mod tests_task;`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::tests::{PROGRAM_SCENE, REQUEST, fade, last_cut, obs, pool, reply, set, spec_of, store};
use super::*;
use crate::obs::remote_call::RemoteCall;
use crate::obs::{ObsCommand, ObsEvent};
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE};
use crate::playback::program_transition::{SpecSource, TransitionKind};
use crate::remote::Upstream;

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
    let fake = fake_obs(replies, capacity);
    let bus = Arc::new(ProgramBus::new());
    let follow = Follow::new(pool.clone(), bus.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let task = tokio::spawn(run_follow_task(follow, fake.upstream, rx, poll));
    TaskRig {
        pool: pool.clone(),
        bus,
        events: fake.events,
        seen: fake.seen,
        program_scene: fake.program_scene,
        shutdown,
        task,
    }
}

/// A fake cg OBS behind the OBS client's command channel (see [`start`]).
struct FakeObs {
    upstream: Upstream,
    events: broadcast::Sender<ObsEvent>,
    seen: mpsc::UnboundedReceiver<String>,
    program_scene: Arc<std::sync::Mutex<String>>,
    /// Events the fake broadcasts, in order, while it answers the NEXT
    /// `GetCurrentSceneTransition` (before its reply): what reaches the queue
    /// while the task waits for that read.
    during_read: Arc<std::sync::Mutex<Vec<ObsEvent>>>,
}

fn fake_obs(replies: Vec<Value>, capacity: usize) -> FakeObs {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(16);
    let (events, _) = broadcast::channel::<ObsEvent>(capacity);
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let program_scene = Arc::new(std::sync::Mutex::new("Slido".to_string()));
    let current = program_scene.clone();
    let during_read = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pending_events = during_read.clone();
    let broadcast_tx = events.clone();
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
                        let queued: Vec<ObsEvent> =
                            std::mem::take(&mut *pending_events.lock().unwrap());
                        for event in queued {
                            let _ = broadcast_tx.send(event);
                        }
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
    let upstream = Upstream::new(Some(cmd_tx), events.clone());
    assert!(upstream.is_configured());
    FakeObs {
        upstream,
        events,
        seen,
        program_scene,
        during_read,
    }
}

/// Every call the fake cg OBS answered so far (no wait).
fn answered(seen: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(r) = seen.try_recv() {
        out.push(r);
    }
    out
}

#[tokio::test]
async fn a_resync_drops_the_queued_scene_changes_older_than_its_read() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let bus = Arc::new(ProgramBus::new());
    let settings = load_follow_settings(&pool).await.unwrap();
    let mut task = FollowLoop::new(
        Follow::new(pool.clone(), bus.clone()),
        fake.upstream.clone(),
        settings,
    );
    // cg OBS shows sp-fast now; still queued: an older switch to sp-slow.
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.events
        .send(ObsEvent::SceneChanged {
            scene_name: "sp-slow".to_string(),
            active_playlist_ids: set(&[8]),
        })
        .expect("subscribed");
    task.resync(&mut events).await;
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (Some(7), 1),
        "only the read scene is followed, never the stale one"
    );
    assert_eq!(
        st.transition.active.map(|w| w.n_slots),
        Some(15),
        "with the transition read first"
    );
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
    // A queued Disconnected: cg OBS is away, so nothing is read or cut, but
    // the spec is still applied.
    store(&pool, "program_transition", "cut").await;
    task.settings = load_follow_settings(&pool).await.unwrap();
    fake.events
        .send(ObsEvent::Disconnected)
        .expect("subscribed");
    task.resync(&mut events).await;
    assert!(!task.obs_up);
    assert_eq!(answered(&mut fake.seen), Vec::<String>::new());
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Cut, 0, 0, SpecSource::Setting)
    );
    assert_eq!(bus.status().health.cuts, 1);
    // cg OBS is back: a queued Connected lets the resync read again.
    fake.events.send(ObsEvent::Connected).expect("subscribed");
    task.resync(&mut events).await;
    assert!(task.obs_up);
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
    assert_eq!(bus.status().health.cuts, 1, "sp-fast is already on program");
}

#[tokio::test]
async fn a_poll_retries_an_unanswered_read_only_while_cg_obs_is_up() {
    let pool = pool().await;
    let failed = json!({
        "requestType": REQUEST,
        "requestStatus": { "result": false, "code": 207 },
    });
    let mut fake = fake_obs(vec![failed, fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let bus = Arc::new(ProgramBus::new());
    let mut task = FollowLoop::new(
        Follow::new(pool.clone(), bus.clone()),
        fake.upstream.clone(),
        FollowSettings::default(),
    );
    task.read_transition(false).await;
    assert!(task.read_pending, "no answer yet");
    task.obs_up = false;
    task.on_tick(&mut events).await;
    assert_eq!(answered(&mut fake.seen), vec![REQUEST], "away: no retry");
    task.obs_up = true;
    task.on_tick(&mut events).await;
    assert!(!task.read_pending, "the retry was answered");
    assert_eq!(answered(&mut fake.seen), vec![REQUEST]);
    task.on_tick(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        Vec::<String>::new(),
        "answered: no more retries"
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 500, 15, SpecSource::Obs)
    );
}

/// A `FollowLoop` over `pool`'s stored settings and a fresh bus.
async fn follow_loop(pool: &SqlitePool, upstream: Upstream) -> (FollowLoop, Arc<ProgramBus>) {
    let bus = Arc::new(ProgramBus::new());
    let settings = load_follow_settings(pool).await.unwrap();
    let task = FollowLoop::new(Follow::new(pool.clone(), bus.clone()), upstream, settings);
    (task, bus)
}

#[tokio::test]
async fn without_an_obs_client_nothing_is_read_retried_or_caught_up() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let (sender, _) = broadcast::channel::<ObsEvent>(4);
    let no_obs = Upstream::new(None, sender);
    let mut events = no_obs.subscribe();
    let (mut task, bus) = follow_loop(&pool, no_obs).await;
    assert!(!task.obs_up, "no OBS client: cg OBS is never up");
    task.resync(&mut events).await;
    assert!(
        !task.read_pending,
        "nothing was asked, so nothing is retried"
    );
    task.on_tick(&mut events).await;
    assert!(!task.read_pending);
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback),
        "the spec is still applied"
    );
    assert_eq!(bus.status().health.cuts, 0);
}

#[tokio::test]
async fn a_live_event_says_cg_obs_is_up_when_its_connected_was_lost() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    // The task saw cg OBS go away; the Connected of its return was then lost
    // in a lag, so only a later event of the new connection is still queued.
    task.obs_up = false;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.events
        .send(ObsEvent::SceneChanged {
            scene_name: "sp-fast".to_string(),
            active_playlist_ids: set(&[7]),
        })
        .expect("subscribed");
    task.resync(&mut events).await;
    assert!(task.obs_up, "only a live connection sends a scene change");
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
    assert_eq!(bus.status().source, Some(7));
    // A Disconnected after a live event still says cg OBS is away.
    fake.events
        .send(ObsEvent::Raw {
            event_type: "InputVolumeChanged".to_string(),
            event_data: json!({}),
        })
        .expect("subscribed");
    fake.events
        .send(ObsEvent::Disconnected)
        .expect("subscribed");
    task.resync(&mut events).await;
    assert!(!task.obs_up);
    assert_eq!(answered(&mut fake.seen), Vec::<String>::new());
}

#[tokio::test]
async fn switching_the_follow_on_while_cg_obs_is_away_asks_nothing() {
    let pool = pool().await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    task.on_event(ObsEvent::Disconnected).await;
    assert!(!task.obs_up);
    store(&pool, "program_follow_obs", "true").await;
    task.on_tick(&mut events).await;
    assert!(task.settings.follow_obs, "switched on");
    assert_eq!(
        answered(&mut fake.seen),
        Vec::<String>::new(),
        "cg OBS is away: its scene is not asked (the reconnect reports it)"
    );
    assert_eq!(bus.status().health.cuts, 0);
}

#[tokio::test]
async fn a_catch_up_drops_the_scene_changes_queued_during_the_transition_read() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    // cg OBS shows sp-fast; an older switch to sp-slow reaches the queue
    // while the task waits for the transition read.
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.during_read
        .lock()
        .unwrap()
        .push(ObsEvent::SceneChanged {
            scene_name: "sp-slow".to_string(),
            active_playlist_ids: set(&[8]),
        });
    task.resync(&mut events).await;
    // Whatever is still queued is handled next, as the task's loop would.
    while let Ok(event) = events.try_recv() {
        task.on_event(event).await;
    }
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (Some(7), 1),
        "only the scene read by the catch-up is followed, never the older one"
    );
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
}

#[tokio::test]
async fn a_transition_change_during_the_read_is_read_again_before_the_catch_up_cuts() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500), fade(2000)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.during_read.lock().unwrap().push(ObsEvent::Raw {
        event_type: "CurrentSceneTransitionChanged".to_string(),
        event_data: json!({}),
    });
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"],
        "read again before the scene"
    );
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    let st = bus.status();
    assert_eq!(st.source, Some(7));
    assert_eq!(
        st.transition.active.map(|w| w.n_slots),
        Some(60),
        "the catch-up cut fades with the 2000 ms transition read again"
    );
}

#[tokio::test]
async fn events_lost_during_the_read_read_the_transition_again_before_the_catch_up() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    // The broadcast keeps ONE event: three sent during the read lag the task.
    let mut fake = fake_obs(vec![fade(500), fade(2000)], 1);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    for event_type in [
        "InputVolumeChanged",
        "InputVolumeChanged",
        "SceneItemEnableStateChanged",
    ] {
        fake.during_read.lock().unwrap().push(ObsEvent::Raw {
            event_type: event_type.to_string(),
            event_data: json!({}),
        });
    }
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"],
        "the lost events may have changed the transition: read again"
    );
    assert_eq!(bus.status().transition.active.map(|w| w.n_slots), Some(60));
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
        answered(&mut self.seen)
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
    let mut rig = start(&pool, vec![fade(500), fade(1000)], 1, NO_POLL);
    assert_eq!(
        rig.next_requests(3).await,
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:Slido"],
        "the transition, then the start's catch-up (a manual scene: no cut)"
    );
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    // cg OBS switched to sp-fast and to a 1000 ms fade, and its events are
    // among the missed ones: the broadcast keeps ONE event, so three sent
    // before the task runs again lag it by two. The one left (a transition
    // change) is older than the re-read, so it is dropped with the rest.
    rig.switch_program_scene("sp-fast");
    rig.raw("SceneItemEnableStateChanged");
    rig.raw("InputVolumeChanged");
    rig.raw("CurrentSceneTransitionChanged");
    assert_eq!(
        rig.next_requests(3).await,
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"],
        "the lag re-read the transition once and caught up"
    );
    rig.source_becomes(7).await;
    let st = rig.bus.status();
    assert_eq!(
        st.transition.active.map(|w| w.n_slots),
        Some(30),
        "the catch-up cut fades with the 1000 ms transition the lag re-read"
    );
    rig.stop().await;
}

#[tokio::test]
async fn switching_the_follow_on_with_a_new_transition_cuts_with_the_new_one() {
    let pool = pool().await;
    store(&pool, "program_transition", "cut").await;
    let mut rig = start(&pool, vec![fade(500)], 16, Duration::from_millis(20));
    rig.switch_program_scene("sp-fast");
    assert_eq!(rig.next_request().await, REQUEST);
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    // ONE save switches the follow on and picks a 400 ms fade.
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES \
         ('program_follow_obs', 'true'), ('program_transition', 'fade'), \
         ('program_transition_ms', '400') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    rig.source_becomes(7).await;
    assert_eq!(
        rig.bus.status().transition.active.map(|w| w.n_slots),
        Some(12),
        "the catch-up cut fades with the 400 ms saved together with the switch"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_transition_read_is_retried_only_while_cg_obs_is_connected() {
    let pool = pool().await;
    let failed = json!({
        "requestType": REQUEST,
        "requestStatus": { "result": false, "code": 207, "comment": "not ready" },
    });
    // The start's read is answered; every later one fails.
    let mut rig = start(
        &pool,
        vec![fade(500), failed],
        16,
        Duration::from_millis(20),
    );
    assert_eq!(rig.next_request().await, REQUEST, "the start's read");
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    // A transition change whose read fails: the polls retry it while cg OBS
    // is up.
    rig.raw("CurrentSceneTransitionChanged");
    assert_eq!(
        rig.next_requests(2).await,
        vec![REQUEST, REQUEST],
        "the event's read, then a poll's retry"
    );
    // cg OBS goes away: the OBS client serves no call then, so a retry would
    // only wait in its queue. The task handles the Disconnected before its
    // next poll (events come first), so once a poll has applied the next
    // setting, no retry follows.
    rig.events
        .send(ObsEvent::Disconnected)
        .expect("the task listens");
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    rig.more_requests(); // the retries that were already running
    store(&pool, "program_transition", "fade").await;
    rig.spec_becomes((TransitionKind::Fade, 300, 9, SpecSource::Setting))
        .await;
    assert_eq!(
        rig.more_requests(),
        Vec::<String>::new(),
        "no retry while cg OBS is away"
    );
    // It is back: it is read again at once.
    rig.events
        .send(ObsEvent::Connected)
        .expect("the task listens");
    assert_eq!(rig.next_request().await, REQUEST);
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
