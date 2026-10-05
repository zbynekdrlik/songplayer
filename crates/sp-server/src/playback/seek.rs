//! The engine's seek (`EngineCommand::Seek`, the dashboard's seek bar): the
//! pipeline seeks the current song, and the song's title clock follows it
//! (#217, residual 5861473237).

use tokio::time::Instant;
use tracing::info;

use super::PlaybackEngine;
use super::pipeline::PipelineCommand;

impl PlaybackEngine {
    /// Seek to `position_ms` within the currently-playing song on the given
    /// playlist. No-op when no pipeline exists for that playlist; the
    /// pipeline's own Seek handler ignores it when no song is loaded.
    ///
    /// #217: the song's title clock re-anchors on the seek
    /// (`TitleClock::seeked`): the hide point moves to the song's new position,
    /// the show point stays. Its timers are re-armed from it, and the wall's
    /// title is re-synced (`resync_after_play`: on program, as the wall owner),
    /// so a seek into the last 3.5 s takes the title down at once and a seek
    /// back reopens its window; the driver acts only on a difference. A paused
    /// song is left alone: its resume's `Started` fixes a new clock. A seek sent
    /// before the song's `Started` (no clock yet, its first ~0.3 s) is applied
    /// by the pipeline right after `Started`, and the clock then counts from the
    /// Play's start: a known corner. So is a seek the decoder refuses: the
    /// pipeline only warns and plays on, while the clock already follows the
    /// asked position (the pipeline reports no seek result).
    pub async fn seek(&mut self, playlist_id: i64, position_ms: u64) {
        let now = Instant::now();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        pp.pipeline.send(PipelineCommand::Seek { position_ms });
        if pp.paused_at.is_some() {
            return;
        }
        let Some(clock) = pp.title_clock else {
            return;
        };
        pp.title_clock = Some(clock.seeked(now, pp.cached_duration_ms, position_ms));
        info!(
            playlist_id,
            video_id = clock.video_id,
            position_ms,
            "seek: the title clock follows the song's new position"
        );
        self.arm_title_timers(playlist_id, now);
        self.resync_after_play(playlist_id).await;
    }
}
