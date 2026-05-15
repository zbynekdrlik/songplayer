//! Manual /play dispatch + pause snapshot accessor for `PlaybackEngine`. #88.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap.
//!
//! Contract: `Pause` (via `PlayAction::Pause` in `execute_action`) captures
//! `(current_video_id, cached_position_ms)` into `PlaylistPipeline::paused_at`.
//! `handle_engine_play` (invoked from `lib.rs` on `EngineCommand::Play`)
//! consumes the snapshot — if `Some`, resumes the same video at the recorded
//! position via `handle_play_video`; otherwise falls back to the prior
//! scene-on dispatch so fresh starts still pick a new video.
//! `handle_play_video` clears any stale `paused_at` so picking a different
//! setlist row after pause doesn't keep the old snapshot.

use super::PlaybackEngine;

impl PlaybackEngine {
    /// Consume paused snapshot for `playlist_id`; `None` if never paused. #88.
    pub fn take_paused_snapshot(&mut self, playlist_id: i64) -> Option<(i64, u64)> {
        self.pipelines
            .get_mut(&playlist_id)
            .and_then(|pp| pp.paused_at.take())
    }

    /// Manual /play: resume paused video if snapshot present, else scene-on. #88.
    pub async fn handle_engine_play(&mut self, playlist_id: i64) {
        match self.take_paused_snapshot(playlist_id) {
            Some((video_id, position_ms)) => {
                self.handle_play_video(playlist_id, video_id, Some(position_ms))
                    .await;
            }
            None => {
                self.handle_scene_change(playlist_id, true).await;
            }
        }
    }
}
