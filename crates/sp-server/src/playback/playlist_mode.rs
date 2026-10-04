//! #225 unit 2: a playlist's playback mode has ONE persisted truth, its
//! `playlists.playback_mode` row. Every pipeline starts in its row's mode
//! (`db::models_playlists::row_mode`, passed in by `startup_senders.rs` and
//! `runtime_pipeline.rs`). A change writes the row first, then tells the
//! engine (`api/routes_mode.rs` → `EngineCommand::SetMode` →
//! `handle_command` → `PlaybackEngine::apply_mode`). Own module:
//! `playback/mod.rs` sits near the 1000-line cap.

use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;

use super::PlaybackEngine;

impl PlaybackEngine {
    /// A mode the playlist's row now holds: its pipeline plays it, and the
    /// dashboards are told, through the one state sender (`broadcast_state`).
    /// A playlist with no pipeline (inactive, or no NDI name) is told `Idle`
    /// in that mode, the state the on-connect replay tells for it, so the
    /// Player follows the change there too.
    pub(super) fn apply_mode(&mut self, playlist_id: i64, mode: PlaybackMode) {
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.mode = mode;
            self.broadcast_state(playlist_id);
            return;
        }
        self.send_dashboard(ServerMsg::PlaybackStateChanged {
            playlist_id,
            state: PlaybackState::Idle,
            mode,
            transport: TransportState::Idle,
        });
    }
}

#[cfg(test)]
#[path = "playlist_mode_tests.rs"]
mod tests;
