//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Re-emit ShowTitle + the subtitle state after a Resolume host recovers.

use std::sync::atomic::Ordering;

use tracing::info;

use super::state::PlayState;
use super::title;

impl super::PlaybackEngine {
    /// Re-emit current state to a recovered Resolume host: ShowTitle for
    /// every active playlist inside its title window (the show timer ran, the
    /// end-of-song hide has not) + the wall's subtitle state (ShowSubtitles for
    /// each on-program line, one HideSubtitles when there is none — also
    /// when no SongPlayer playlist is on program).
    pub(crate) async fn handle_resolume_recovery(&self, host: &str) {
        info!(
            host,
            "Resolume recovery — re-emitting current state for active pipelines"
        );
        let mut shows = Vec::new();
        for (&playlist_id, pp) in &self.pipelines {
            let PlayState::Playing { video_id } = pp.state else {
                continue;
            };
            if !pp.scene_active.load(Ordering::Acquire) {
                continue;
            }
            // Re-show the title only inside its window (#217 addendum 2).
            // A pending show timer (Started + 1.5 s) shows it itself: a
            // second ShowTitle restarts the fade. A finished end-of-song
            // hide must stay: re-showing would carry the title into the next
            // song (a HideTitle that 404'd on a stale clip map ends in
            // exactly this recovery). `cancel_title_timers` `take()`s both
            // handles, so an aborted one never lingers.
            let show_pending = pp
                .title_show_abort
                .as_ref()
                .is_some_and(|show| !show.is_finished());
            let hide_done = pp
                .title_hide_abort
                .as_ref()
                .is_some_and(|hide| hide.is_finished());
            if !show_pending
                && !hide_done
                && title::push_title(
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
            let lines = pp.lyrics_state.as_ref().and_then(|state| {
                state.resolume_lines_with_next(pp.cached_position_ms, pp.cached_lyrics_reference)
            });
            if let Some((en, next_en, sk, next_sk)) = lines {
                let cmd = crate::resolume::ResolumeCommand::ShowSubtitles {
                    en,
                    next_en,
                    sk,
                    next_sk,
                    suppress_en: pp.cached_suppress_en,
                };
                shows.push((playlist_id, video_id, cmd));
            }
        }
        // Re-emit the wall's CURRENT subtitle state, a blank one included (a
        // blank plan position, a song without lyrics, or no SongPlayer
        // playlist on program). The engine's own hide for it
        // (`dispatch_lyrics_if_changed`, `clear_lyrics_display` at song
        // start, or the scene-off hide) was skipped against the host's empty
        // clip map and is not re-sent, so without this a stale text Arena
        // restored from its saved composition would stay. The subtitle clips
        // are shared by every on-program playlist, so the one Hide goes out
        // only when none of them has a line. The clear is instant; the title
        // is not hidden here, as `hide_title` fades from full opacity and
        // would flash a stale title that is already hidden (#217).
        if shows.is_empty() {
            let _ = self
                .resolume_tx
                .send(crate::resolume::ResolumeCommand::HideSubtitles)
                .await;
            info!(
                host,
                "subtitles cleared on Resolume recovery — no on-program line"
            );
        }
        for (playlist_id, video_id, cmd) in shows {
            let _ = self.resolume_tx.send(cmd).await;
            info!(
                playlist_id,
                video_id, "subtitle re-pushed on Resolume recovery"
            );
        }
    }
}
