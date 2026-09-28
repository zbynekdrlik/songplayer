//! #221 L3: the events the facade emits ITSELF (design record 5873773896
//! §1a): Companion's program feedback and the transition events come from
//! SongPlayer's own program, never from cg OBS's.
//!
//! - [`run_program_feedback`] (one per bound listener, started by
//!   `remote::serve`) watches what the program bus publishes as on air, names
//!   it with the one resolver (`program_scene_name`) and emits
//!   `CurrentProgramSceneChanged {sceneName}` (intent Scenes) whenever the
//!   NAME changes: after a Companion press, a dashboard cut, the OBS follow,
//!   or anything else that cuts. A publication under the same name (a
//!   same-scene press, the re-kick) is no event, as in OBS.
//! - A facade switch that cut `SP-program` ([`announce_transition`], called
//!   by the session for `SetCurrentProgramScene` and
//!   `TriggerStudioModeTransition`) emits `SceneTransitionStarted`, then
//!   `SceneTransitionEnded` once the program's transition window is served
//!   (`transition.active` clears): at once when nothing is mixed (a Cut, a
//!   same-source cut), else bounded by [`TRANSITION_END_MAX_WAIT`]. Both
//!   carry intent Transitions. A dashboard cut emits no transition event.
//!
//! Every event goes to every session on the facade's own broadcast
//! (`Facade::events`); a session delivers one only when its client subscribed
//! to the event's intent.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

use super::protocol::{EVENT_SCENES, EVENT_TRANSITIONS};
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::{OnAir, program_scene_name};
use crate::playback::program_transition::TransitionKind;

/// How many of the facade's own events a slow session may fall behind before
/// it misses some (it then logs how many).
pub const FACADE_EVENTS_CAPACITY: usize = 64;
/// How often the `SceneTransitionEnded` waiter reads the program state.
pub const TRANSITION_END_POLL: Duration = Duration::from_millis(20);
/// The longest the `SceneTransitionEnded` waiter waits. The longest window
/// the bus opens is ~10.6 s (the 10 s fade cap after the one-slot lead and
/// the cue gate's 15-slot wait), so only a stalled `SP-program` sender reaches
/// it; `SceneTransitionEnded` is then sent anyway (with a WARN), so a client
/// never waits forever.
pub const TRANSITION_END_MAX_WAIT: Duration = Duration::from_secs(15);

/// `SceneTransitionStarted`.
pub const TRANSITION_STARTED: &str = "SceneTransitionStarted";
/// `SceneTransitionEnded`.
pub const TRANSITION_ENDED: &str = "SceneTransitionEnded";
/// `CurrentProgramSceneChanged`.
pub const PROGRAM_SCENE_CHANGED: &str = "CurrentProgramSceneChanged";

/// One event the facade emits itself.
#[derive(Clone, Debug, PartialEq)]
pub struct FacadeEvent {
    pub event_type: &'static str,
    /// The `EventSubscription` bit a client must have subscribed to.
    pub intent: u64,
    pub data: Value,
}

impl FacadeEvent {
    /// `CurrentProgramSceneChanged {sceneName}` (Scenes).
    pub fn program_scene(scene: &str) -> Self {
        Self {
            event_type: PROGRAM_SCENE_CHANGED,
            intent: EVENT_SCENES,
            data: json!({ "sceneName": scene }),
        }
    }

    /// `SceneTransitionStarted` / `SceneTransitionEnded {transitionName}`
    /// (Transitions).
    pub fn transition(event_type: &'static str, transition_name: &str) -> Self {
        Self {
            event_type,
            intent: EVENT_TRANSITIONS,
            data: json!({ "transitionName": transition_name }),
        }
    }
}

/// The scene name to announce: `now`, when there is one and it differs from
/// the name announced `last`.
pub fn scene_change<'a>(last: Option<&str>, now: Option<&'a str>) -> Option<&'a str> {
    now.filter(|scene| last != Some(*scene))
}

/// The start of a listener's program feedback: subscribe to what is on air
/// and name it NOW. Every publication after this is a change
/// [`run_program_feedback`] sees, even one made before the task first runs
/// (review round 1: a press in that window was never announced).
pub fn subscribe_program(bus: &ProgramBus) -> (watch::Receiver<OnAir>, Option<String>) {
    let mut on_air = bus.on_air();
    let name = program_scene_name(&on_air.borrow_and_update());
    (on_air, name)
}

/// Emit `CurrentProgramSceneChanged` for every change of the program's scene
/// name until the bus goes away (the task is dropped with its listener). The
/// value `on_air` holds when the task starts is not announced: a client reads
/// the program when it connects.
pub async fn run_program_feedback(
    mut on_air: watch::Receiver<OnAir>,
    _last: Option<String>,
    events: broadcast::Sender<FacadeEvent>,
) {
    let mut last = program_scene_name(&on_air.borrow_and_update());
    while on_air.changed().await.is_ok() {
        let now = program_scene_name(&on_air.borrow_and_update());
        if let Some(scene) = scene_change(last.as_deref(), now.as_deref()) {
            debug!(
                scene,
                "remote: the program scene changed — fed back to the clients"
            );
            // No receiver (no client connected) is not an error.
            let _ = events.send(FacadeEvent::program_scene(scene));
        }
        last = now;
    }
}

/// The transition's name in the transition events (`transitionName`): the
/// kind the program cuts with.
pub fn transition_name(kind: TransitionKind) -> &'static str {
    match kind {
        TransitionKind::Cut => "Cut",
        TransitionKind::Fade => "Fade",
    }
}

/// A facade switch cut `SP-program`: emit `SceneTransitionStarted` now and
/// `SceneTransitionEnded` once the program's transition window is served
/// (see the module doc). Started is sent before the waiter exists, so every
/// client gets it before Ended.
pub fn announce_transition(bus: &Arc<ProgramBus>, events: &broadcast::Sender<FacadeEvent>) {
    let name = transition_name(bus.status().transition.kind);
    let _ = events.send(FacadeEvent::transition(TRANSITION_STARTED, name));
    tokio::spawn(end_transition(
        Arc::clone(bus),
        events.clone(),
        name,
        TRANSITION_END_POLL,
        TRANSITION_END_MAX_WAIT,
    ));
}

/// Send `SceneTransitionEnded` once the window is over, or after `max`.
async fn end_transition(
    bus: Arc<ProgramBus>,
    events: broadcast::Sender<FacadeEvent>,
    name: &'static str,
    poll: Duration,
    max: Duration,
) {
    log_transition_end(wait_transition_end(&bus, poll, max).await, max);
    let _ = events.send(FacadeEvent::transition(TRANSITION_ENDED, name));
}

/// Wait until no transition window is running on `SP-program`
/// (`transition.active` is `None`), reading it every `poll`, at most `max`.
/// `true` = it ended (at once when nothing is mixed); `false` = `max` ran out.
pub async fn wait_transition_end(bus: &ProgramBus, poll: Duration, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        if bus.status().transition.active.is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// The log line of a transition's end (`ended` = the window was served).
/// Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_transition_end(ended: bool, max: Duration) {
    if !ended {
        warn!(
            max_s = max.as_secs(),
            "remote: the program's transition window did not end in time — SceneTransitionEnded sent anyway"
        );
    }
}

#[cfg(test)]
#[path = "studio_events_tests.rs"]
mod tests;
