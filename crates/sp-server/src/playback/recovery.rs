//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Re-emit ShowTitle + the subtitle state after a Resolume host recovers.

use std::sync::atomic::Ordering;

use tracing::info;

use super::state::PlayState;
use super::title;

impl super::PlaybackEngine {
    /// Re-emit current state to a recovered Resolume host: ShowTitle for
    /// every active playlist + its subtitle state (ShowSubtitles for the
    /// current line, HideSubtitles when the wall is blank).
    pub(crate) async fn handle_resolume_recovery(&self, host: &str) {
        info!(
            host,
            "Resolume recovery — re-emitting current state for active pipelines"
        );
        for (&playlist_id, pp) in &self.pipelines {
            let PlayState::Playing { video_id } = pp.state else {
                continue;
            };
            if !pp.scene_active.load(Ordering::Acquire) {
                continue;
            }
            if title::push_title(
                &self.pool,
                self.obs_cmd_tx.as_ref(),
                &self.resolume_tx,
                video_id,
            )
            .await
            {
                info!(
                    playlist_id,
                    video_id, "title re-pushed on Resolume recovery"
                );
            }
            // Re-emit the wall's CURRENT subtitle state, a blank one included:
            // a blank plan position, or a song without lyrics. The engine's
            // own hide for it (`dispatch_lyrics_if_changed`, or
            // `clear_lyrics_display` at song start) was skipped against the
            // host's empty clip map and is not re-sent, so without this a
            // stale text Arena restored from its saved composition would stay
            // until the next line, or the whole song (#217).
            let lines = pp.lyrics_state.as_ref().and_then(|state| {
                state.resolume_lines_with_next(pp.cached_position_ms, pp.cached_lyrics_reference)
            });
            let cmd = match lines {
                Some((en, next_en, sk, next_sk)) => {
                    crate::resolume::ResolumeCommand::ShowSubtitles {
                        en,
                        next_en,
                        sk,
                        next_sk,
                        suppress_en: pp.cached_suppress_en,
                    }
                }
                None => crate::resolume::ResolumeCommand::HideSubtitles,
            };
            let blank = matches!(cmd, crate::resolume::ResolumeCommand::HideSubtitles);
            let _ = self.resolume_tx.send(cmd).await;
            info!(
                playlist_id,
                video_id, blank, "subtitle re-pushed on Resolume recovery"
            );
        }
    }
}
