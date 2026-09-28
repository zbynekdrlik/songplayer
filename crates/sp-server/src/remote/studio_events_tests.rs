//! #221 L3 `remote/studio_events.rs`: the name-change rule, the program
//! feedback task over a real `ProgramBus`, the event shapes, and the
//! transition-end waiter + announcement on a paused clock (the bus's window
//! is ended the way the `SP-program` sender does it, `release_due`).
//! Wired via `#[cfg(test)] #[path = "studio_events_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::Instant;

use super::*;
use crate::playback::program_bus::CUT_LEAD_SLOTS;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_transition::{
    CUE_WAIT_MAX_SLOTS, MAX_TRANSITION_SLOTS, SpecSource, TransitionKind, TransitionSpec,
};
use crate::playback::wallclock::utc_now_100ns;

const POLL: Duration = Duration::from_millis(20);
const MAX: Duration = Duration::from_secs(15);
/// One minute in 100 ns units: every boundary up to it is due.
const MINUTE_100NS: i64 = 600_000_000;

/// The next facade event (bounded).
async fn recv(rx: &mut broadcast::Receiver<FacadeEvent>) -> FacadeEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("no event within the timeout")
        .expect("the channel closed")
}

/// Let the spawned tasks of this current-thread runtime run until idle.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// A bus with playlist 3 on air, the 300 ms fade in force, cut to 7: a fade
/// window nobody serves yet.
fn fading_bus() -> Arc<ProgramBus> {
    let bus = Arc::new(ProgramBus::new());
    assert!(bus.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
    bus.select_initial(3, Some("sp-slow"));
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    assert!(bus.status().transition.active.is_some());
    bus
}

#[test]
fn only_a_new_name_is_announced() {
    assert_eq!(scene_change(None, Some("sp-fast")), Some("sp-fast"));
    assert_eq!(
        scene_change(Some("sp-slow"), Some("sp-fast")),
        Some("sp-fast")
    );
    assert_eq!(scene_change(Some("sp-fast"), Some("sp-fast")), None);
    assert_eq!(scene_change(Some("sp-fast"), None), None);
    assert_eq!(scene_change(None, None), None);
}

#[test]
fn the_events_have_obs_websockets_shapes() {
    assert_eq!(
        FacadeEvent::program_scene("sp-fast"),
        FacadeEvent {
            event_type: "CurrentProgramSceneChanged",
            intent: 4,
            data: json!({ "sceneName": "sp-fast" }),
        }
    );
    assert_eq!(
        FacadeEvent::transition(TRANSITION_STARTED, "Fade"),
        FacadeEvent {
            event_type: "SceneTransitionStarted",
            intent: 16,
            data: json!({ "transitionName": "Fade" }),
        }
    );
    assert_eq!(TRANSITION_ENDED, "SceneTransitionEnded");
    // Review round 3: the production Ended bound outlasts the longest window
    // the bus opens (~10.6 s: the 10 s fade cap after the lead + cue wait).
    assert_eq!(TRANSITION_END_MAX_WAIT, Duration::from_secs(15));
    // Round 4: pin that relation, not only the literal — the lead slot, the
    // cue gate's wait and the longest fade, plus one slot to be served.
    let slots = CUT_LEAD_SLOTS as u64 + u64::from(CUE_WAIT_MAX_SLOTS + MAX_TRANSITION_SLOTS) + 1;
    let longest = Duration::from_millis(slots * 1000 / sp_core::genlock::GENLOCK_GRID_FPS as u64);
    assert!(TRANSITION_END_MAX_WAIT > longest, "{longest:?}");
    assert_eq!(transition_name(TransitionKind::Cut), "Cut");
    assert_eq!(transition_name(TransitionKind::Fade), "Fade");
}

#[tokio::test]
async fn the_feedback_announces_every_change_of_the_program_scene_name() {
    let bus = Arc::new(ProgramBus::new());
    // On air before the task starts: not announced (a client reads the
    // program when it connects).
    bus.select_initial(3, Some("sp-slow"));
    let (tx, mut rx) = broadcast::channel(16);
    let (on_air, last) = subscribe_program(&bus);
    assert_eq!(last.as_deref(), Some("sp-slow"));
    let task = tokio::spawn(run_program_feedback(on_air, last, tx));
    settle().await;
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    assert_eq!(recv(&mut rx).await, FacadeEvent::program_scene("sp-fast"));
    // Published again under the same name (a same-scene press): no event.
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    settle().await;
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    // A manual scene, then the NDI input with no scene (the resolver's name).
    bus.cut(-1, utc_now_100ns(), Some("Slido"));
    assert_eq!(recv(&mut rx).await, FacadeEvent::program_scene("Slido"));
    bus.cut(-1, utc_now_100ns(), None);
    assert_eq!(
        recv(&mut rx).await,
        FacadeEvent::program_scene("OBS manuál")
    );
    // The task ends with the bus.
    drop(bus);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the feedback outlived the bus")
        .unwrap();
}

/// Review round 1: a cut after the listener subscribed but before its
/// feedback task first ran (a client can press right after the listener
/// starts) is still announced.
#[tokio::test]
async fn a_cut_before_the_feedback_task_first_runs_is_announced() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(3, Some("sp-slow"));
    let (tx, mut rx) = broadcast::channel(16);
    let (on_air, last) = subscribe_program(&bus);
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    let _task = tokio::spawn(run_program_feedback(on_air, last, tx));
    assert_eq!(recv(&mut rx).await, FacadeEvent::program_scene("sp-fast"));
}

#[tokio::test(start_paused = true)]
async fn the_end_of_a_cut_is_at_once() {
    let bus = ProgramBus::new();
    bus.select_initial(3, None);
    bus.cut(7, utc_now_100ns(), None); // a fresh bus cuts with a Cut
    let start = Instant::now();
    assert!(wait_transition_end(&bus, POLL, MAX).await);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn the_wait_ends_when_the_window_is_served() {
    let bus = fading_bus();
    let waiter = tokio::spawn({
        let bus = Arc::clone(&bus);
        async move {
            let start = Instant::now();
            let ended = wait_transition_end(&bus, POLL, MAX).await;
            (ended, start.elapsed())
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!waiter.is_finished(), "ended before the window was served");
    bus.release_due(utc_now_100ns() + MINUTE_100NS);
    assert!(bus.status().transition.active.is_none());
    let (ended, waited) = waiter.await.unwrap();
    assert!(ended);
    assert!(
        waited >= Duration::from_secs(1) && waited <= Duration::from_secs(1) + POLL,
        "{waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_window_nobody_serves_ends_the_wait_after_max() {
    let bus = fading_bus();
    let start = Instant::now();
    assert!(!wait_transition_end(&bus, POLL, MAX).await);
    let waited = start.elapsed();
    assert!(waited >= MAX && waited < MAX + POLL, "{waited:?}");
}

#[tokio::test(start_paused = true)]
async fn a_switch_announces_started_now_and_ended_once_the_window_is_served() {
    let bus = fading_bus();
    let (tx, mut rx) = broadcast::channel(16);
    announce_transition(&bus, &tx, TRANSITION_END_MAX_WAIT);
    // Started is sent before the waiter exists.
    assert_eq!(
        rx.try_recv(),
        Ok(FacadeEvent::transition(TRANSITION_STARTED, "Fade"))
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rx.try_recv(),
        Err(TryRecvError::Empty),
        "not before the window ends"
    );
    bus.release_due(utc_now_100ns() + MINUTE_100NS);
    assert_eq!(
        recv(&mut rx).await,
        FacadeEvent::transition(TRANSITION_ENDED, "Fade")
    );
}

#[tokio::test(start_paused = true)]
async fn a_window_nobody_serves_is_announced_ended_after_the_max_wait() {
    let bus = fading_bus();
    let (tx, mut rx) = broadcast::channel(16);
    let start = Instant::now();
    announce_transition(&bus, &tx, TRANSITION_END_MAX_WAIT);
    assert_eq!(
        rx.try_recv(),
        Ok(FacadeEvent::transition(TRANSITION_STARTED, "Fade"))
    );
    // Not `recv`: its 10 s bound is shorter than the 15 s it must outwait
    // (the clock is paused, so this longer bound costs no real time).
    let ended = tokio::time::timeout(TRANSITION_END_MAX_WAIT * 2, rx.recv())
        .await
        .expect("no Ended within twice the max wait")
        .expect("the channel closed");
    assert_eq!(ended, FacadeEvent::transition(TRANSITION_ENDED, "Fade"));
    let waited = start.elapsed();
    assert!(
        waited >= TRANSITION_END_MAX_WAIT && waited < TRANSITION_END_MAX_WAIT + TRANSITION_END_POLL,
        "{waited:?}"
    );
}
