//! #215: `FollowLoop`'s own steps, driven one by one against the fake cg OBS
//! of `program_follow_tests_task.rs` (no task runs, so what the fake logged
//! is complete when a step returns): the drains (stale scene changes, a
//! transition change or a lag during a read, the bounded re-reads, the
//! fallback to the newest dropped scene change and when it is forgotten), cg
//! OBS's connection state,
//! and the retry and the switch-on catch-up gated on cg OBS being up.
//! Wired via `#[cfg(test)] #[path = "program_follow_tests_loop.rs"] mod tests_loop;`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

use super::tests::{PROGRAM_SCENE, REQUEST, fade, last_cut, pool, set, spec_of, store};
use super::tests_task::{answered, fake_obs};
use super::*;
use crate::obs::ObsEvent;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_transition::{SpecSource, TransitionKind};
use crate::remote::Upstream;

/// A `FollowLoop` over `pool`'s stored settings and a fresh bus.
async fn follow_loop(pool: &SqlitePool, upstream: Upstream) -> (FollowLoop, Arc<ProgramBus>) {
    let bus = Arc::new(ProgramBus::new());
    let settings = load_follow_settings(pool).await.unwrap();
    let task = FollowLoop::new(Follow::new(pool.clone(), bus.clone()), upstream, settings);
    (task, bus)
}

/// cg OBS's program scene changed to `name` (the OBS client's derived event).
fn scene_event(name: &str, playlists: &[i64]) -> ObsEvent {
    ObsEvent::SceneChanged {
        scene_name: name.to_string(),
        active_playlist_ids: set(playlists),
    }
}

/// A cg OBS event of `event_type`.
fn raw_event(event_type: &str) -> ObsEvent {
    ObsEvent::Raw {
        event_type: event_type.to_string(),
        event_data: json!({}),
    }
}

#[tokio::test]
async fn a_resync_drops_the_queued_scene_changes_older_than_its_read() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    // cg OBS shows sp-fast now; still queued: an older switch to sp-slow.
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.events
        .send(scene_event("sp-slow", &[8]))
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
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
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
        .send(scene_event("sp-fast", &[7]))
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
        .send(raw_event("InputVolumeChanged"))
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
        .push_back(vec![scene_event("sp-slow", &[8])]);
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
    fake.during_read
        .lock()
        .unwrap()
        .push_back(vec![raw_event("CurrentSceneTransitionChanged")]);
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
async fn a_scene_change_queued_during_that_reread_is_dropped_too() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500), fade(2000)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    // The first read brings a transition change, the re-read an older scene
    // change: a drain still comes right before the scene read.
    fake.during_read.lock().unwrap().extend([
        vec![raw_event("CurrentSceneTransitionChanged")],
        vec![scene_event("sp-slow", &[8])],
    ]);
    task.resync(&mut events).await;
    while let Ok(event) = events.try_recv() {
        task.on_event(event).await;
    }
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (Some(7), 1),
        "never back to the older scene"
    );
    assert_eq!(st.transition.active.map(|w| w.n_slots), Some(60));
}

#[tokio::test]
async fn a_transition_still_changing_after_three_rereads_is_left_to_the_polls() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let replies = [500, 1000, 1500, 2000, 2500].map(fade).to_vec();
    let mut fake = fake_obs(replies, 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    // cg OBS changes its transition during each of four reads.
    fake.during_read
        .lock()
        .unwrap()
        .extend((0..4).map(|_| vec![raw_event("CurrentSceneTransitionDurationChanged")]));
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![
            REQUEST,
            REQUEST,
            REQUEST,
            REQUEST,
            PROGRAM_SCENE,
            "ScenePlaylists:sp-fast"
        ],
        "the start's read and three re-reads, then the scene"
    );
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(
        bus.status().transition.active.map(|w| w.n_slots),
        Some(60),
        "the cut fades with the last read (2000 ms)"
    );
    assert!(task.read_pending, "still changing: a poll reads it again");
    task.on_tick(&mut events).await;
    assert_eq!(answered(&mut fake.seen), vec![REQUEST]);
    assert!(!task.read_pending);
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 2500, 75, SpecSource::Obs)
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
    fake.during_read.lock().unwrap().push_back(vec![
        raw_event("InputVolumeChanged"),
        raw_event("InputVolumeChanged"),
        raw_event("SceneItemEnableStateChanged"),
    ]);
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"],
        "the lost events may have changed the transition: read again"
    );
    assert_eq!(bus.status().transition.active.map(|w| w.n_slots), Some(60));
}

#[tokio::test]
async fn an_unanswered_catch_up_follows_the_newest_scene_change_it_dropped() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    // Two scene changes reach the queue while the task waits for the
    // transition read; then cg OBS does not answer the scene read.
    fake.during_read.lock().unwrap().push_back(vec![
        scene_event("sp-fast", &[7]),
        scene_event("sp-slow", &[8]),
    ]);
    fake.scene_unanswered.store(true, Ordering::SeqCst);
    task.resync(&mut events).await;
    assert_eq!(answered(&mut fake.seen), vec![REQUEST, PROGRAM_SCENE]);
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (Some(8), 1),
        "the newest dropped scene change is followed"
    );
    assert_eq!(last_cut(&bus).scene, "sp-slow");
    // An answered catch-up follows the answer, never the dropped change, and
    // forgets it: a later unanswered one has nothing to fall back on.
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.scene_unanswered.store(false, Ordering::SeqCst);
    fake.during_read
        .lock()
        .unwrap()
        .push_back(vec![scene_event("sp-slow", &[8])]);
    task.resync(&mut events).await;
    assert_eq!(bus.status().source, Some(7));
    fake.scene_unanswered.store(true, Ordering::SeqCst);
    task.resync(&mut events).await;
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (Some(7), 2),
        "nothing was dropped this time: nothing to follow"
    );
}

#[tokio::test]
async fn a_catch_up_whose_scene_lookup_fails_follows_only_a_dropped_change_of_that_scene() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    *fake.program_scene.lock().unwrap() = "sp-fast".to_string();
    fake.lookup_unanswered.store(true, Ordering::SeqCst);
    // cg OBS names sp-fast but not its playlists; the dropped change is of
    // sp-slow, so it is older than sp-fast.
    fake.during_read
        .lock()
        .unwrap()
        .push_back(vec![scene_event("sp-slow", &[8])]);
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, PROGRAM_SCENE, "ScenePlaylists:sp-fast"]
    );
    let st = bus.status();
    assert_eq!(
        (st.source, st.health.cuts),
        (None, 0),
        "an older scene is never followed"
    );
    // A dropped change of sp-fast itself carries its playlists.
    fake.during_read
        .lock()
        .unwrap()
        .push_back(vec![scene_event("sp-fast", &[7])]);
    task.resync(&mut events).await;
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (Some(7), 1));
    assert_eq!(last_cut(&bus).scene, "sp-fast");
}

#[tokio::test]
async fn a_dropped_scene_change_is_forgotten_after_a_reconnect() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let mut fake = fake_obs(vec![fade(500)], 16);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    // A scene change of the previous connection, then a new connection: it
    // re-reports its own scene, so the old change is not followed even when
    // cg OBS does not name its scene.
    fake.events
        .send(scene_event("sp-slow", &[8]))
        .expect("subscribed");
    fake.events.send(ObsEvent::Connected).expect("subscribed");
    fake.scene_unanswered.store(true, Ordering::SeqCst);
    task.resync(&mut events).await;
    assert_eq!(answered(&mut fake.seen), vec![REQUEST, PROGRAM_SCENE]);
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (None, 0));
}

#[tokio::test]
async fn a_dropped_scene_change_is_forgotten_after_a_lag() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    // The broadcast keeps ONE event: the queued scene change, then three
    // events during the read lag the task (a newer scene change may be lost).
    let mut fake = fake_obs(vec![fade(500)], 1);
    let mut events = fake.upstream.subscribe();
    let (mut task, bus) = follow_loop(&pool, fake.upstream.clone()).await;
    fake.events
        .send(scene_event("sp-slow", &[8]))
        .expect("subscribed");
    fake.during_read.lock().unwrap().push_back(vec![
        raw_event("InputVolumeChanged"),
        raw_event("InputVolumeChanged"),
        raw_event("InputVolumeChanged"),
    ]);
    fake.scene_unanswered.store(true, Ordering::SeqCst);
    task.resync(&mut events).await;
    assert_eq!(
        answered(&mut fake.seen),
        vec![REQUEST, REQUEST, PROGRAM_SCENE],
        "the lag re-read the transition; the scene read got no answer"
    );
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (None, 0));
}
