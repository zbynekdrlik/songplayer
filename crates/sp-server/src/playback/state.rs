//! Pure playback state machine.
//!
//! The [`PlayState::transition`] function is a pure function: given the current
//! state, an event, and the current playback mode it returns the next state and
//! an optional action for the engine to execute.  No I/O, no side-effects.

use sp_core::playback::PlaybackMode;

/// Logical state of a single playlist player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayState {
    /// No videos available yet.
    Idle,
    /// Videos are available but the scene is not on program.
    WaitingForScene,
    /// Actively playing a video.
    Playing { video_id: i64 },
}

/// Events that drive state transitions.
#[derive(Debug, Clone)]
pub enum PlayEvent {
    /// At least one normalised video is ready to play.
    VideosAvailable,
    /// The playlist's NDI source appeared on the OBS program output.
    SceneOn,
    /// The playlist's NDI source left the OBS program output.
    SceneOff,
    /// The current video reached its end.
    VideoEnded,
    /// The current video encountered a playback error.
    VideoError(String),
    /// The user requested a skip.
    Skip,
    /// The user changed the playback mode.
    SetMode(PlaybackMode),
}

/// Side-effects that the engine should execute after a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayAction {
    /// Select the next video and start playback.
    SelectAndPlay,
    /// Replay the current video from the beginning.
    ReplayCurrent,
    /// Pause the pipeline (e.g. send black frames).
    Pause,
    /// Send a black frame and stop (used for Single mode after ending).
    SendBlack,
    /// Stop playback entirely.
    Stop,
}

impl PlayState {
    /// Pure transition function.
    ///
    /// Returns `(next_state, optional_action)`.  The caller (the playback
    /// engine) is responsible for executing the action.
    pub fn transition(
        self,
        event: PlayEvent,
        mode: PlaybackMode,
    ) -> (PlayState, Option<PlayAction>) {
        match (&self, &event) {
            // Idle + videos available -> waiting for scene
            (PlayState::Idle, PlayEvent::VideosAvailable) => (PlayState::WaitingForScene, None),

            // Waiting + scene on -> select and play
            (PlayState::WaitingForScene, PlayEvent::SceneOn) => {
                // State stays WaitingForScene until the engine sets Playing
                // after selection succeeds.
                (PlayState::WaitingForScene, Some(PlayAction::SelectAndPlay))
            }

            // Playing + scene off -> pause
            (PlayState::Playing { .. }, PlayEvent::SceneOff) => {
                (PlayState::WaitingForScene, Some(PlayAction::Pause))
            }

            // Playing + video ended -> depends on mode
            (PlayState::Playing { .. }, PlayEvent::VideoEnded) => match mode {
                PlaybackMode::Continuous => {
                    (PlayState::WaitingForScene, Some(PlayAction::SelectAndPlay))
                }
                PlaybackMode::Single => (PlayState::WaitingForScene, Some(PlayAction::SendBlack)),
                PlaybackMode::Loop => (self, Some(PlayAction::ReplayCurrent)),
            },

            // Playing + skip -> select next
            (PlayState::Playing { .. }, PlayEvent::Skip) => {
                (PlayState::WaitingForScene, Some(PlayAction::SelectAndPlay))
            }

            // Playing + error -> skip broken video, select next
            (PlayState::Playing { .. }, PlayEvent::VideoError(_)) => {
                (PlayState::WaitingForScene, Some(PlayAction::SelectAndPlay))
            }

            // Mode change -> no state change, no action (mode is stored externally)
            (_, PlayEvent::SetMode(_)) => (self, None),

            // Default: stay in current state with no action
            _ => (self, None),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_to_waiting_on_videos_available() {
        let (next, action) =
            PlayState::Idle.transition(PlayEvent::VideosAvailable, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, None);
    }

    #[test]
    fn waiting_to_select_on_scene_on() {
        let (next, action) =
            PlayState::WaitingForScene.transition(PlayEvent::SceneOn, PlaybackMode::Continuous);
        // State stays WaitingForScene; engine will set Playing after selection.
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }

    #[test]
    fn playing_to_waiting_on_scene_off() {
        let state = PlayState::Playing { video_id: 1 };
        let (next, action) = state.transition(PlayEvent::SceneOff, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::Pause));
    }

    #[test]
    fn playing_continuous_video_ended_selects_next() {
        let state = PlayState::Playing { video_id: 1 };
        let (next, action) = state.transition(PlayEvent::VideoEnded, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }

    #[test]
    fn playing_single_video_ended_sends_black() {
        let state = PlayState::Playing { video_id: 1 };
        let (next, action) = state.transition(PlayEvent::VideoEnded, PlaybackMode::Single);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SendBlack));
    }

    #[test]
    fn playing_loop_video_ended_replays() {
        let state = PlayState::Playing { video_id: 42 };
        let (next, action) = state.transition(PlayEvent::VideoEnded, PlaybackMode::Loop);
        assert_eq!(next, PlayState::Playing { video_id: 42 });
        assert_eq!(action, Some(PlayAction::ReplayCurrent));
    }

    #[test]
    fn playing_skip_selects_next() {
        let state = PlayState::Playing { video_id: 5 };
        let (next, action) = state.transition(PlayEvent::Skip, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }

    #[test]
    fn playing_error_selects_next() {
        let state = PlayState::Playing { video_id: 3 };
        let (next, action) = state.transition(
            PlayEvent::VideoError("decode error".into()),
            PlaybackMode::Continuous,
        );
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }

    #[test]
    fn idle_ignores_scene_on() {
        let (next, action) =
            PlayState::Idle.transition(PlayEvent::SceneOn, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::Idle);
        assert_eq!(action, None);
    }

    #[test]
    fn waiting_ignores_video_ended() {
        let (next, action) =
            PlayState::WaitingForScene.transition(PlayEvent::VideoEnded, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, None);
    }

    #[test]
    fn mode_change_no_state_change() {
        // From Idle
        let (next, action) = PlayState::Idle.transition(
            PlayEvent::SetMode(PlaybackMode::Loop),
            PlaybackMode::Continuous,
        );
        assert_eq!(next, PlayState::Idle);
        assert_eq!(action, None);

        // From WaitingForScene
        let (next, action) = PlayState::WaitingForScene.transition(
            PlayEvent::SetMode(PlaybackMode::Single),
            PlaybackMode::Continuous,
        );
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, None);

        // From Playing
        let state = PlayState::Playing { video_id: 7 };
        let (next, action) = state.transition(
            PlayEvent::SetMode(PlaybackMode::Continuous),
            PlaybackMode::Loop,
        );
        assert_eq!(next, PlayState::Playing { video_id: 7 });
        assert_eq!(action, None);
    }

    #[test]
    fn idle_ignores_scene_off() {
        let (next, action) =
            PlayState::Idle.transition(PlayEvent::SceneOff, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::Idle);
        assert_eq!(action, None);
    }

    #[test]
    fn idle_ignores_skip() {
        let (next, action) = PlayState::Idle.transition(PlayEvent::Skip, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::Idle);
        assert_eq!(action, None);
    }

    #[test]
    fn idle_ignores_video_error() {
        let (next, action) = PlayState::Idle.transition(
            PlayEvent::VideoError("err".into()),
            PlaybackMode::Continuous,
        );
        assert_eq!(next, PlayState::Idle);
        assert_eq!(action, None);
    }

    #[test]
    fn waiting_ignores_scene_off() {
        let (next, action) =
            PlayState::WaitingForScene.transition(PlayEvent::SceneOff, PlaybackMode::Continuous);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, None);
    }

    #[test]
    fn playing_skip_in_loop_mode() {
        let state = PlayState::Playing { video_id: 10 };
        let (next, action) = state.transition(PlayEvent::Skip, PlaybackMode::Loop);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }

    #[test]
    fn playing_error_in_single_mode() {
        let state = PlayState::Playing { video_id: 2 };
        let (next, action) =
            state.transition(PlayEvent::VideoError("broken".into()), PlaybackMode::Single);
        assert_eq!(next, PlayState::WaitingForScene);
        assert_eq!(action, Some(PlayAction::SelectAndPlay));
    }
}
