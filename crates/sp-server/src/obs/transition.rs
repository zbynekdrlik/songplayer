//! #219: cg OBS's current scene transition, kept by the OBS client itself.
//!
//! Every program cut follows cg OBS's scene transition
//! (`playback::program_transition::effective_spec`, #215). The OBS client
//! reads it on its own connection and keeps it in `ObsState::transition`, so
//! it reaches every consumer through the published `ObsSnapshot`
//! (`snapshot.rs`) — the program follow never asks cg OBS itself.
//!
//! - One reader task per connection ([`run_transition_reader`], spawned at
//!   connect): it reads `GetCurrentSceneTransition` at once, then again each
//!   time it is woken. The connection's reader task wakes it on
//!   `CurrentSceneTransitionChanged` / `CurrentSceneTransitionDurationChanged`
//!   ([`is_transition_event`]; the identify subscribes the Transitions
//!   events for this).
//! - Reads never overlap. A change that arrives during a read keeps the wake
//!   (a `Notify` permit), so it is read again right after: the stored
//!   transition is always the newest answer.
//! - A read with no answer leaves the transition unknown (`None`) and is asked
//!   again every [`TRANSITION_RETRY`] (the scene poll's cadence) until one is
//!   answered. The first failure WARNs, the retries log at debug.
//! - A disconnect forgets it (`ObsState` reset); the program follow keeps the
//!   last one it saw.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::snapshot::ObsShared;
use crate::remote::clip;

/// The obs-websocket request that reads cg OBS's current scene transition.
pub const GET_CURRENT_SCENE_TRANSITION: &str = "GetCurrentSceneTransition";

/// How often a transition read that got no answer is asked again.
pub const TRANSITION_RETRY: Duration = Duration::from_secs(2);

/// cg OBS's current scene transition, as `GetCurrentSceneTransition` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObsTransition {
    /// The transition's name in cg OBS (e.g. `Fade`).
    pub name: String,
    /// Its kind (`fade_transition`, `cut_transition`, `swipe_transition`, …).
    pub kind: String,
    /// Its duration; `None` for a fixed-duration transition.
    pub duration_ms: Option<u32>,
}

/// cg OBS's transition from the op=7 `d` object of `GetCurrentSceneTransition`;
/// `None` when the request failed or carries no kind. cg OBS-chosen names are
/// clipped (`remote::clip`).
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

/// A cg OBS event that changes its current transition (kind or duration).
pub fn is_transition_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "CurrentSceneTransitionChanged" | "CurrentSceneTransitionDurationChanged"
    )
}

/// One connection's transition reader (see the module doc). Runs until the
/// connection loop aborts it with the connection's other tasks.
pub(crate) async fn run_transition_reader(
    write: SharedWrite,
    dispatcher: Dispatcher,
    obs: ObsShared,
    wake: Arc<Notify>,
) {
    let mut failures: u32 = 0;
    loop {
        if read_transition(&write, &dispatcher, &obs, failures).await {
            failures = 0;
            wake.notified().await;
        } else {
            failures = failures.saturating_add(1);
            tokio::select! {
                _ = tokio::time::sleep(TRANSITION_RETRY) => {}
                _ = wake.notified() => {}
            }
        }
    }
}

/// Read cg OBS's transition into `ObsState::transition`; `true` when cg OBS
/// answered. No answer sets it unknown (`None`). `failures` = the reads in a
/// row that got none so far (the first failure WARNs).
async fn read_transition(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    obs: &ObsShared,
    failures: u32,
) -> bool {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = serde_json::json!({
        "op": 6,
        "d": { "requestType": GET_CURRENT_SCENE_TRANSITION, "requestId": req_id.clone() }
    });
    let reply = dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await;
    let read = reply
        .as_ref()
        .ok()
        .and_then(|r| obs_transition_from_reply(&r["d"]));
    let Some(transition) = read else {
        obs.update(|s| s.transition = None).await;
        let error = reply.err().map(|e| e.to_string());
        if failures == 0 {
            warn!(
                error = ?error,
                "obs: cg OBS did not report its scene transition — asked again every 2 s"
            );
        } else {
            debug!(error = ?error, "obs: cg OBS still did not report its scene transition");
        }
        return false;
    };
    let changed = obs
        .update(|s| {
            let changed = s.transition.as_ref() != Some(&transition);
            s.transition = Some(transition.clone());
            changed
        })
        .await;
    if changed {
        info!(
            name = %transition.name,
            kind = %transition.kind,
            duration_ms = ?transition.duration_ms,
            "obs: cg OBS's scene transition"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reply(name: &str, kind: &str, duration: Value) -> Value {
        json!({
            "requestType": GET_CURRENT_SCENE_TRANSITION,
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

    fn obs(name: &str, kind: &str, duration_ms: Option<u32>) -> ObsTransition {
        ObsTransition {
            name: name.to_string(),
            kind: kind.to_string(),
            duration_ms,
        }
    }

    #[test]
    fn cg_obs_transition_is_read_from_its_reply() {
        assert_eq!(
            obs_transition_from_reply(&reply("Fade", "fade_transition", json!(300))),
            Some(obs("Fade", "fade_transition", Some(300)))
        );
        assert_eq!(
            obs_transition_from_reply(&reply("Cut", "cut_transition", Value::Null)),
            Some(obs("Cut", "cut_transition", None)),
            "a fixed transition has no duration"
        );
        assert_eq!(
            obs_transition_from_reply(&reply("Fade", "fade_transition", json!(5_000_000_000_u64)))
                .and_then(|t| t.duration_ms),
            Some(u32::MAX),
            "an absurd duration saturates"
        );
        let failed = json!({
            "requestType": GET_CURRENT_SCENE_TRANSITION,
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
    }

    #[test]
    fn only_the_two_transition_events_wake_the_reader() {
        assert!(is_transition_event("CurrentSceneTransitionChanged"));
        assert!(is_transition_event("CurrentSceneTransitionDurationChanged"));
        assert!(!is_transition_event("SceneTransitionStarted"));
        assert!(!is_transition_event("CurrentProgramSceneChanged"));
        assert!(!is_transition_event(""));
        assert_eq!(TRANSITION_RETRY, Duration::from_secs(2));
    }
}
