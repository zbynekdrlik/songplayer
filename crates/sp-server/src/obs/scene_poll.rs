//! Poll-based reconciliation of the OBS program scene (#170).
//!
//! In Studio Mode OBS can DROP a `CurrentProgramSceneChanged` event
//! (reproduced live, round 3): after a same-scene transition leaves
//! `preview == program`, the next `SetCurrentProgramScene` changes what
//! `GetCurrentProgramScene` reports but fires no event. SongPlayer's event
//! stream stays alive yet never learns of the switch, so the wall sits on a
//! paused source — a dark wall in daily studio-mode operation. The connection
//! loop polls `GetCurrentProgramScene` on a ~2 s cadence and reconciles a
//! mismatch through the same path `CurrentProgramSceneChanged` feeds.

use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::scene::apply_scene_change;
use crate::obs::text::get_current_scene_request;
use crate::obs::{NdiSourceMap, ObsEvent, ObsState, SharedWrite};

/// One poll pass: read `GetCurrentProgramScene` over the existing WS and, when
/// it differs from the last event-derived scene (a dropped
/// `CurrentProgramSceneChanged`, #170), reconcile it through
/// [`apply_scene_change`] — the exact path the event takes. Cheap and
/// best-effort: a closed/timed-out read is transient (the reconnect loop or the
/// next tick handles it), so it is silently ignored rather than logged as an
/// error.
pub async fn reconcile_program_scene(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
) {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_current_scene_request(&req_id);
    let polled = match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(resp) => resp["d"]["responseData"]["currentProgramSceneName"]
            .as_str()
            .map(|s| s.to_string()),
        Err(_) => None,
    };
    let Some(polled) = polled else {
        return;
    };

    let last = { state.read().await.current_scene.clone() };
    if let Some(scene) = scene_poll_detects_change(last.as_deref(), &polled) {
        info!(
            scene = %scene,
            "obs: program scene changed without an event — reconciled by poll"
        );
        apply_scene_change(write, dispatcher, ndi_sources, state, event_tx, scene).await;
    }
}

/// Decide whether a polled program scene reflects a change the event stream
/// missed.
///
/// Returns `Some(polled_scene)` when the polled scene differs from the last
/// event-derived scene (a dropped event to reconcile — including when no event
/// scene is known yet), else `None` (already in sync; no action).
pub fn scene_poll_detects_change(
    last_event_scene: Option<&str>,
    polled_scene: &str,
) -> Option<String> {
    match last_event_scene {
        Some(s) if s == polled_scene => None,
        _ => Some(polled_scene.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_dropped_switch_when_polled_differs_from_last_event() {
        // OBS dropped the CurrentProgramSceneChanged; the poll sees sp-fast
        // while the last event scene is still sp-slow → reconcile to sp-fast.
        assert_eq!(
            scene_poll_detects_change(Some("sp-slow"), "sp-fast"),
            Some("sp-fast".to_string()),
        );
    }

    #[test]
    fn no_change_when_polled_matches_last_event() {
        // Steady state — the event stream and the poll agree → no reconcile.
        assert_eq!(scene_poll_detects_change(Some("sp-fast"), "sp-fast"), None);
    }

    #[test]
    fn reconciles_when_no_event_scene_known_yet() {
        // No CurrentProgramSceneChanged has been seen (or state was reset) but
        // the poll has a scene → adopt it.
        assert_eq!(
            scene_poll_detects_change(None, "sp-fast"),
            Some("sp-fast".to_string()),
        );
    }
}
