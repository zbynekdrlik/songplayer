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

    /// Test-only (#221 L4a): put a pipeline on program the way the dark-wall
    /// check expects a receiver on it (`ndi_health_expect`): its scene is on
    /// program AND cg OBS was told to show it (`legacy_cg` on the engine's
    /// program bus; a fresh bus when none is set yet).
    pub(crate) fn set_on_program_for_test(&mut self, playlist_id: i64) {
        self.set_scene_active_for_test(playlist_id, true);
        self.set_cg_shown_for_test(Some(playlist_id));
    }

    /// Test-only (#221 L4a): record that cg OBS was told to show `shown` (a
    /// playlist, or `None` for a manual scene) on the engine's program bus.
    pub(crate) fn set_cg_shown_for_test(&self, shown: Option<i64>) {
        let bus = self
            .program
            .get_or_init(|| std::sync::Arc::new(super::program_bus::ProgramBus::new()));
        let legacy = bus.legacy_cg();
        let ticket = legacy.ticket();
        legacy.confirmed(ticket, shown);
    }
}
