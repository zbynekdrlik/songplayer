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
    // RED: inverted — reports a change only when the scenes MATCH, and never
    // when no event scene is known yet. GREEN reconciles on a DIFFERENCE.
    match last_event_scene {
        Some(s) if s == polled_scene => Some(polled_scene.to_string()),
        _ => None,
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
