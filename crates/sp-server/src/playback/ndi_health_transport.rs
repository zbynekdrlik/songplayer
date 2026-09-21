//! Pure mapping from an NDI-health snapshot's RAW pipeline label
//! ([`PlaybackStateLabel`], the pipeline's own `reported_state` BEFORE scene
//! reconciliation) to the wire-level [`TransportState`] (#201 round 2).
//!
//! Round 1 carried `transport` on the LIVE `PlaybackStateChanged` message. The
//! on-connect replay, though, derived transport from the health registry's
//! SCENE-RECONCILED label, and `handle_health_snapshot` maps a Playing-but-off-
//! program pipeline to `Paused` — so a dashboard that CONNECTED while an
//! off-program dub decoded (a reload, or a play clicked before the app socket
//! opened) replayed `▶ Prehrať` for a playing dub until the next live message.
//!
//! Round 2 stores the transport derived from the RAW `reported_state` on the
//! snapshot, and the replay reads it directly — no second mapping in
//! `websocket.rs`. `Playing` is the only decoding label; `WaitingForScene` and
//! `Paused` are both not-decoding (paused), and `Idle` is no content.

use sp_core::playback::TransportState;

use super::ndi_health::PlaybackStateLabel;

/// Map a pipeline's RAW [`PlaybackStateLabel`] (`reported_state`) to its own
/// [`TransportState`], INDEPENDENTLY of whether its scene is on OBS program.
pub(crate) fn transport_from_reported(state: &PlaybackStateLabel) -> TransportState {
    match state {
        // A decoding pipeline is Playing regardless of whether its scene is on
        // OBS program — so an off-program decoding dub replays `transport:
        // Playing` and a reloaded dashboard reads `⏸ Pauza`.
        PlaybackStateLabel::Playing => TransportState::Playing,
        // A Playing-off-program pipeline the registry reconciled to `Paused`,
        // and `WaitingForScene` (black-framed / awaiting its scene), are both
        // not decoding — paused.
        PlaybackStateLabel::Paused => TransportState::Paused,
        PlaybackStateLabel::WaitingForScene => TransportState::Paused,
        // No video loaded.
        PlaybackStateLabel::Idle => TransportState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_playing_maps_to_playing() {
        assert_eq!(
            transport_from_reported(&PlaybackStateLabel::Playing),
            TransportState::Playing
        );
    }

    #[test]
    fn reported_paused_maps_to_paused() {
        assert_eq!(
            transport_from_reported(&PlaybackStateLabel::Paused),
            TransportState::Paused
        );
    }

    #[test]
    fn reported_waiting_for_scene_maps_to_paused() {
        assert_eq!(
            transport_from_reported(&PlaybackStateLabel::WaitingForScene),
            TransportState::Paused
        );
    }

    #[test]
    fn reported_idle_maps_to_idle() {
        assert_eq!(
            transport_from_reported(&PlaybackStateLabel::Idle),
            TransportState::Idle
        );
    }
}
