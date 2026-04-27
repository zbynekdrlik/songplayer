//! Test-only helpers on `PlaybackEngine`.
//! Extracted from `mod.rs` to keep that file under the 1000-line cap.

#[cfg(test)]
use super::PlaybackEngine;
#[cfg(test)]
use super::state::PlayState;

#[cfg(test)]
impl PlaybackEngine {
    /// Test-only: force a pipeline's canonical engine state. Lets the
    /// ndi_health unit tests drive the WaitingForScene override path
    /// without spinning up an OBS event stream.
    pub(crate) fn set_state_for_test(&mut self, playlist_id: i64, state: PlayState) {
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.state = state;
        }
    }

    /// Test-only: force a pipeline's `scene_active` flag. Lets the
    /// ndi_health unit tests drive both branches of the new
    /// "Playing+scene_active=false" gate.
    pub(crate) fn set_scene_active_for_test(&mut self, playlist_id: i64, active: bool) {
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.scene_active
                .store(active, std::sync::atomic::Ordering::Release);
        }
    }
}
