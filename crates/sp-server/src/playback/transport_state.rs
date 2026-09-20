//! Pure mapping from the engine's raw [`PlayState`] to the wire-level
//! [`TransportState`] (#201).
//!
//! `TransportState` answers only "is this pipeline decoding right now?",
//! INDEPENDENTLY of whether its NDI output is on OBS program. It sits beside
//! `state: WsPlaybackState` on `PlaybackStateChanged`: `play_state_to_ws` keeps
//! folding the on/off-program fact into `state` (a decoding-off-program pipeline
//! is reported `WaitingForScene`, unchanged — the #170 scene-aware contract),
//! while this function reports the transport from the RAW `PlayState` so the
//! shared Player's play/pause label follows the pipeline, not the program.
//!
//! `PlayState` has no literal `Paused` variant; `WaitingForScene` is the state a
//! pipeline enters when its scene leaves program (`SceneOff` -> the `Pause`
//! action, black frames) or while awaiting its scene — in both it is NOT
//! decoding, i.e. paused, distinct from `Idle` (no video loaded).

use sp_core::playback::TransportState;

use super::state::PlayState;

/// Map the raw engine [`PlayState`] to the pipeline's own [`TransportState`].
pub(crate) fn transport_from_play_state(state: &PlayState) -> TransportState {
    match state {
        // A decoding pipeline is Playing regardless of whether its scene is on
        // program — this is the whole point of #201 (an off-program dub reads
        // `⏸ Pauza` while it plays).
        PlayState::Playing { .. } => TransportState::Playing,
        PlayState::WaitingForScene => TransportState::Paused,
        PlayState::Idle => TransportState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playing_maps_to_playing() {
        assert_eq!(
            transport_from_play_state(&PlayState::Playing { video_id: 42 }),
            TransportState::Playing
        );
    }

    #[test]
    fn waiting_for_scene_maps_to_paused() {
        assert_eq!(
            transport_from_play_state(&PlayState::WaitingForScene),
            TransportState::Paused
        );
    }

    #[test]
    fn idle_maps_to_idle() {
        assert_eq!(
            transport_from_play_state(&PlayState::Idle),
            TransportState::Idle
        );
    }
}
