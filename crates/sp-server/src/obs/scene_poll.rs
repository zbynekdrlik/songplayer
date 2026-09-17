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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    pending: &Mutex<Option<(String, Instant)>>,
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
    // The mismatch clock lives across ticks: `(polled scene, first seen)`.
    let verdict = {
        let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = guard
            .as_ref()
            .filter(|(scene, _)| scene == &polled)
            .map(|(_, since)| since.elapsed());
        let verdict = scene_poll_verdict(last.as_deref(), &polled, elapsed, SCENE_POLL_CONFIRM);
        *guard = match verdict {
            PollVerdict::Pending if elapsed.is_none() => Some((polled.clone(), Instant::now())),
            PollVerdict::Pending => guard.take(),
            PollVerdict::InSync | PollVerdict::Reconcile(_) => None,
        };
        verdict
    };
    if let PollVerdict::Reconcile(scene) = verdict {
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

/// How long a polled mismatch must persist before the poll reconciles it —
/// the `CurrentProgramSceneChanged` event fires at the END of the Studio-Mode
/// transition (2000 ms fade on the box), while `GetCurrentProgramScene` reports
/// the target at its START; acting earlier freezes the outgoing scene mid-fade.
pub(crate) const SCENE_POLL_CONFIRM: Duration = Duration::from_millis(3000);

/// The poll's verdict for one tick.
#[derive(Debug, PartialEq, Eq)]
pub enum PollVerdict {
    /// Event stream and poll agree — nothing to do.
    InSync,
    /// Mismatch seen, but the event still has time to arrive — wait.
    Pending,
    /// Mismatch persisted past the confirm window — reconcile to this scene.
    Reconcile(String),
}

/// Decide the poll's action from the last event-derived scene, the polled
/// scene, how long THIS mismatch has been pending (`None` = first sighting) and
/// the confirm window. Pure — unit-tested.
pub fn scene_poll_verdict(
    last_event_scene: Option<&str>,
    polled_scene: &str,
    pending_for: Option<Duration>,
    confirm: Duration,
) -> PollVerdict {
    let _ = (pending_for, confirm);
    match scene_poll_detects_change(last_event_scene, polled_scene) {
        None => PollVerdict::InSync,
        Some(scene) => PollVerdict::Reconcile(scene),
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

    // ---- confirm window (#170 round 4): the poll must not beat the event ----

    #[test]
    fn mismatch_first_sighting_is_pending() {
        // First tick that sees a mismatch: the transition just started; the
        // event fires at its END, so wait — never reconcile on first sight.
        assert_eq!(
            scene_poll_verdict(Some("sp-slow"), "sp-fast", None, Duration::from_secs(3)),
            PollVerdict::Pending
        );
    }

    #[test]
    fn mismatch_inside_confirm_window_stays_pending() {
        assert_eq!(
            scene_poll_verdict(
                Some("sp-slow"),
                "sp-fast",
                Some(Duration::from_millis(1000)),
                Duration::from_secs(3)
            ),
            PollVerdict::Pending
        );
    }

    #[test]
    fn mismatch_past_confirm_window_reconciles() {
        // The event never came (a dropped studio-mode switch) → reconcile.
        assert_eq!(
            scene_poll_verdict(
                Some("sp-slow"),
                "sp-fast",
                Some(Duration::from_millis(3500)),
                Duration::from_secs(3)
            ),
            PollVerdict::Reconcile("sp-fast".to_string())
        );
    }

    #[test]
    fn in_sync_is_in_sync_whatever_was_pending() {
        assert_eq!(
            scene_poll_verdict(
                Some("sp-fast"),
                "sp-fast",
                Some(Duration::from_secs(9)),
                Duration::from_secs(3)
            ),
            PollVerdict::InSync
        );
    }

    #[test]
    fn confirm_window_outlasts_the_studio_fade() {
        // The box fade is 2000 ms; the event fires at its END.
        assert!(SCENE_POLL_CONFIRM >= Duration::from_millis(2500));
    }
}
