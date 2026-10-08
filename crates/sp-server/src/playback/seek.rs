//! The engine's seek (`EngineCommand::Seek`, the dashboard's seek bar): the
//! pipeline seeks the current song and reports where it really plays on from
//! (`PipelineEvent::Seeked`), and the song's title clock follows that report
//! (#217, residual 5861473237 and its follow-up 5987205883).

use tokio::time::Instant;
use tracing::{debug, info};

use super::PlaybackEngine;
use super::pipeline::PipelineCommand;

impl PlaybackEngine {
    /// Seek to `position_ms` within the currently-playing song on the given
    /// playlist. No-op when no pipeline exists for that playlist; the
    /// pipeline's own Seek handler ignores it when no song is loaded.
    ///
    /// #217: nothing else moves here. The decoder may refuse the seek and
    /// play on from where it was, so the song's title clock waits for the
    /// pipeline's report of where the song really plays from
    /// ([`seeked`](Self::seeked)), the same contract as a Play and its
    /// `Started`. Not async: it only sends the command.
    pub fn seek(&mut self, playlist_id: i64, position_ms: u64) {
        let Some(pp) = self.pipelines.get(&playlist_id) else {
            return;
        };
        pp.pipeline.send(PipelineCommand::Seek { position_ms });
        info!(
            playlist_id,
            position_ms, "seek: sent to the pipeline (the title clock follows its report)"
        );
    }

    /// `PipelineEvent::Seeked` of `playlist_id`: its song plays on from
    /// `position_ms` (#217). The song's title clock re-anchors on it
    /// (`TitleClock::seeked`): the hide point moves to the song's real
    /// position, the show point stays. Its timers are re-armed from it, and
    /// the wall's title is re-synced (`resync_after_play`: on program, as the
    /// wall owner), so a seek into the last 3.5 s takes the title down at once
    /// and a seek back reopens its window; the driver acts only on a
    /// difference. A seek sent before the song's `Started` is applied by the
    /// pipeline after it, so its report moves the new clock.
    ///
    /// Ignored, with nothing sent:
    /// - for a paused song: its resume's `Started` fixes a new clock;
    /// - with no clock: a newer Play cleared it (`begin_play`), so the report
    ///   is an earlier song's (the pipeline sends it before the next song's
    ///   `Started`, never after).
    pub(super) async fn seeked(&mut self, playlist_id: i64, position_ms: u64) {
        let now = Instant::now();
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            return;
        };
        if pp.paused_at.is_some() {
            debug!(
                playlist_id,
                position_ms, "seek report of a paused song — ignored"
            );
            return;
        }
        let Some(clock) = pp.title_clock else {
            debug!(
                playlist_id,
                position_ms, "seek report with no title clock (an earlier song's) — ignored"
            );
            return;
        };
        pp.title_clock = Some(clock.seeked(now, pp.cached_duration_ms, position_ms));
        info!(
            playlist_id,
            video_id = clock.video_id,
            position_ms,
            "seek: the title clock follows the song's real position"
        );
        self.arm_title_timers(playlist_id, now);
        self.resync_after_play(playlist_id).await;
    }
}
