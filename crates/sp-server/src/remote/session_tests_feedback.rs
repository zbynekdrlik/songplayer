//! #221 L3: the facade's program feedback and transition events are
//! SongPlayer's own, over REAL sockets (the rig of `session_tests.rs`):
//! `CurrentProgramSceneChanged` for every change of SP-program's scene name
//! (a press AND a dashboard cut), `SceneTransitionStarted` →
//! `SceneTransitionEnded` around a facade switch that cut (at once for a Cut,
//! only once the window is served for a fade), `GetCurrentProgramScene`
//! answered from SP-program, and `remote.program_scene`. cg OBS's own
//! program-scene event is never passed through (`session_tests.rs`).
//! Wired via `#[cfg(test)] #[path = "session_tests_feedback.rs"] mod tests_feedback;`.

use std::time::Duration;

use serde_json::{Value, json};

use super::tests::{
    Client, connect, enable_input, hello_identify, next_json, press, request, rig, send_json,
};
use crate::playback::program_switch::{Via, switch_source};
use crate::playback::program_transition::{SpecSource, TransitionSpec};
use crate::playback::wallclock::utc_now_100ns;

/// `EventSubscription::Scenes`.
const SCENES: u64 = 4;
/// `EventSubscription::Transitions`.
const TRANSITIONS: u64 = 16;

/// Send one request and read until its response: its RequestResponse `d`
/// and the events (their `d`) that arrived before it. A session delivers an
/// event already queued before it answers the next request (`biased`), so
/// `before` holds every event emitted before the request was sent.
pub(super) async fn request_collecting(
    ws: &mut Client,
    request_type: &str,
    data: Option<Value>,
) -> (Value, Vec<Value>) {
    let id = format!("collect-{request_type}");
    let mut d = json!({ "requestType": request_type, "requestId": id });
    if let Some(data) = data {
        d["requestData"] = data;
    }
    send_json(ws, json!({ "op": 6, "d": d })).await;
    let mut before = Vec::new();
    loop {
        let msg = next_json(ws).await;
        if msg["op"] == 7 {
            assert_eq!(msg["d"]["requestId"], id, "{msg}");
            return (msg["d"].clone(), before);
        }
        assert_eq!(msg["op"], 5, "only events may come first: {msg}");
        before.push(msg["d"].clone());
    }
}

/// The next message, which must be an event: its `d`.
async fn next_event(ws: &mut Client) -> Value {
    let msg = next_json(ws).await;
    assert_eq!(msg["op"], 5, "expected an event, got {msg}");
    msg["d"].clone()
}

/// The next `n` events (their `d`).
async fn next_events(ws: &mut Client, n: usize) -> Vec<Value> {
    let mut events = Vec::with_capacity(n);
    for _ in 0..n {
        events.push(next_event(ws).await);
    }
    events
}

/// The position of the one event of `event_type` in `events`.
fn position_of(events: &[Value], event_type: &str) -> usize {
    let at: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["eventType"] == event_type)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "exactly one {event_type} in {events:?}");
    at[0]
}

fn program_scene_changed(scene: &str) -> Value {
    json!({
        "eventType": "CurrentProgramSceneChanged",
        "eventIntent": 4,
        "eventData": { "sceneName": scene },
    })
}

fn transition_event(event_type: &str, name: &str) -> Value {
    json!({
        "eventType": event_type,
        "eventIntent": 16,
        "eventData": { "transitionName": name },
    })
}

#[tokio::test]
async fn a_press_is_fed_back_as_songplayers_own_program_scene() {
    let rig = rig().await;
    let mut presser = connect(rig.addr).await;
    hello_identify(&mut presser, SCENES).await;
    let mut other = connect(rig.addr).await;
    hello_identify(&mut other, SCENES).await;
    let mut general = connect(rig.addr).await;
    hello_identify(&mut general, 1).await;
    // The press is answered first; its feedback follows, under the catalog's
    // name whatever case was pressed.
    let d = press(&mut presser, "SP-Fast").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    let expected = program_scene_changed("sp-fast");
    assert_eq!(next_event(&mut presser).await, expected);
    assert_eq!(next_event(&mut other).await, expected);
    // A Scenes-only client never gets the transition events (Transitions).
    let (_, before) = request_collecting(&mut presser, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "{before:?}");
    // A General-only client gets no event at all.
    let (_, before) = request_collecting(&mut general, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(rig.remote().program_scene.as_deref(), Some("sp-fast"));
}

#[tokio::test]
async fn a_dashboard_cut_is_fed_back_with_no_transition_events() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, SCENES | TRANSITIONS).await;
    // The dashboard's cut path (`POST /api/v1/program/cut`, #221 L4a:
    // `switch_source`, via=dashboard), not the facade.
    switch_source(&rig.pool, &rig.bus, 3, Via::Dashboard)
        .await
        .unwrap();
    assert_eq!(next_event(&mut ws).await, program_scene_changed("sp-slow"));
    // No transition event: the next message is a later request's response.
    let (_, before) = request_collecting(&mut ws, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(rig.remote().program_scene.as_deref(), Some("sp-slow"));
    // "OBS manuál" with no scene is named by the resolver.
    switch_source(&rig.pool, &rig.bus, -1, Via::Dashboard)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut ws).await,
        program_scene_changed("OBS manuál")
    );
    assert_eq!(rig.remote().program_scene.as_deref(), Some("OBS manuál"));
}

#[tokio::test]
async fn a_cut_press_announces_its_transition_started_then_ended_at_once() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, SCENES | TRANSITIONS).await;
    // A fresh bus cuts with a Cut: nothing is mixed, so Ended follows at once.
    let d = press(&mut ws, "sp-fast").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    let events = next_events(&mut ws, 3).await;
    let started = position_of(&events, "SceneTransitionStarted");
    let ended = position_of(&events, "SceneTransitionEnded");
    assert!(started < ended, "{events:?}");
    assert_eq!(
        events[started],
        transition_event("SceneTransitionStarted", "Cut")
    );
    assert_eq!(
        events[ended],
        transition_event("SceneTransitionEnded", "Cut")
    );
    let scene = position_of(&events, "CurrentProgramSceneChanged");
    assert_eq!(events[scene], program_scene_changed("sp-fast"));
}

#[tokio::test]
async fn the_same_scene_again_is_a_transition_but_no_program_scene_event() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, SCENES | TRANSITIONS).await;
    press(&mut ws, "sp-fast").await;
    next_events(&mut ws, 3).await; // the first press: started, scene, ended
    let seq = rig.bus.on_air_now().seq;
    // The transition to the scene already on air: published again (the
    // re-kick), announced as a transition, but the NAME did not change.
    let d = request(&mut ws, "TriggerStudioModeTransition", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.on_air_now().seq, seq + 1, "published again");
    let events = next_events(&mut ws, 2).await;
    assert_eq!(
        events,
        [
            transition_event("SceneTransitionStarted", "Cut"),
            transition_event("SceneTransitionEnded", "Cut"),
        ]
    );
    let (_, before) = request_collecting(&mut ws, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "no program scene event: {before:?}");
}

#[tokio::test]
async fn a_fade_ends_only_once_its_window_is_served() {
    let rig = rig().await;
    assert!(
        rig.bus
            .set_transition(TransitionSpec::fade(300, SpecSource::Setting))
    );
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, TRANSITIONS).await;
    let d = press(&mut ws, "sp-fast").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(
        next_event(&mut ws).await,
        transition_event("SceneTransitionStarted", "Fade")
    );
    assert!(rig.bus.status().transition.active.is_some());
    // No sender serves the window yet, so no Ended. The "not yet" window only
    // errs in the safe direction.
    let early = tokio::time::timeout(Duration::from_millis(200), next_json(&mut ws)).await;
    assert!(early.is_err(), "Ended before the window was served");
    // The program's sender serves it: every boundary up to a minute ahead is
    // due, and with no source live they are filled, which ends the window.
    rig.bus.release_due(utc_now_100ns() + 600_000_000);
    assert!(rig.bus.status().transition.active.is_none());
    assert_eq!(
        next_event(&mut ws).await,
        transition_event("SceneTransitionEnded", "Fade")
    );
    // A Transitions-only client gets no program scene event.
    let (_, before) = request_collecting(&mut ws, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "{before:?}");
}

#[tokio::test]
async fn a_press_that_cuts_nothing_announces_no_transition() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, SCENES | TRANSITIONS).await;
    // cg OBS refuses an unknown manual scene: nothing is cut.
    let d = press(&mut ws, "Nope").await;
    assert_eq!(d["requestStatus"]["code"], 600);
    // A manual scene cg OBS switched to while the input is off: kept.
    enable_input(&rig.pool, false).await;
    let d = press(&mut ws, "Slido").await;
    assert_eq!(d["requestStatus"]["code"], 100);
    assert_eq!(rig.bus.status().source, None);
    let (_, before) = request_collecting(&mut ws, "GetStudioModeEnabled", None).await;
    assert!(before.is_empty(), "{before:?}");
}

#[tokio::test]
async fn get_current_program_scene_is_sp_programs_own_scene() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    // Nothing on program: 604, like obs-websocket's InvalidResourceState.
    let d = request(&mut ws, "GetCurrentProgramScene", None).await;
    assert_eq!(d["requestStatus"]["code"], 604);
    assert_eq!(d["requestStatus"]["result"], false);
    rig.bus.select_initial(3, Some("sp-slow"));
    let d = request(&mut ws, "GetCurrentProgramScene", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    assert_eq!(
        d["responseData"],
        json!({ "sceneName": "sp-slow", "currentProgramSceneName": "sp-slow" })
    );
    rig.bus.select_initial(-1, None);
    let d = request(&mut ws, "GetCurrentProgramScene", None).await;
    assert_eq!(d["responseData"]["currentProgramSceneName"], "OBS manuál");
    assert!(
        rig.calls().is_empty(),
        "cg OBS is never asked: {:?}",
        rig.calls()
    );
}

/// #221 lane 2 (ROZHODNUTÉ 6002459249): Companion's feedback at CONNECT
/// comes from `GetSceneList` (`currentProgramSceneName` → `scene_active`,
/// `currentPreviewSceneName` → `scene_preview`). cg OBS's own program
/// (sp-slow in the fake) is wrong whenever a playlist is on SP-program, so
/// the forwarded answer names SP-program's scene and this session's preview,
/// each with cg OBS's uuid of that scene (null for a name cg OBS does not
/// list); the scene list itself is cg OBS's.
#[tokio::test]
async fn get_scene_list_names_sp_program_s_scene_and_this_session_s_preview() {
    let rig = rig().await;
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;

    // A playlist on SP-program; no preview set: the preview is the program.
    rig.bus.select_initial(7, Some("sp-fast"));
    let d = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(d["requestStatus"], json!({ "result": true, "code": 100 }));
    let data = &d["responseData"];
    assert_eq!(data["currentProgramSceneName"], "sp-fast");
    assert_eq!(data["currentProgramSceneUuid"], "u-sp-fast");
    assert_eq!(data["currentPreviewSceneName"], "sp-fast");
    assert_eq!(data["currentPreviewSceneUuid"], "u-sp-fast");
    let names: Vec<&str> = data["scenes"]
        .as_array()
        .expect("cg OBS's scene list")
        .iter()
        .map(|s| s["sceneName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sp-fast", "sp-slow", "Slido", "Trailer"]);

    // This session's own preview.
    let set = request(
        &mut ws,
        "SetCurrentPreviewScene",
        Some(json!({ "sceneName": "Slido" })),
    )
    .await;
    assert_eq!(set["requestStatus"]["code"], 100);
    let d = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(d["responseData"]["currentProgramSceneName"], "sp-fast");
    assert_eq!(d["responseData"]["currentPreviewSceneName"], "Slido");
    assert_eq!(d["responseData"]["currentPreviewSceneUuid"], "u-Slido");

    // "OBS manuál" with no scene name: cg OBS lists no such scene.
    rig.bus.select_initial(-1, None);
    let d = request(&mut ws, "GetSceneList", None).await;
    assert_eq!(d["responseData"]["currentProgramSceneName"], "OBS manuál");
    assert!(
        d["responseData"]["currentProgramSceneUuid"].is_null(),
        "{d}"
    );
    assert_eq!(d["responseData"]["currentPreviewSceneName"], "Slido");
}
