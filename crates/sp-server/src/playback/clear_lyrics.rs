//! `clear_lyrics_display` — extracted from `playback/mod.rs` to keep that
//! file under the 1000-line cap.  Pure delegate; same method, same behavior.

use sp_core::ws::ServerMsg;

impl super::PlaybackEngine {
    /// Send empty lyrics to dashboard, Resolume AND Presenter to clear
    /// stale display when the previous song ends or the operator switches
    /// to a song without lyrics. Without the Presenter clear, the stage
    /// display kept showing the last line of the previous song until the
    /// next song's first line pushed — cue for singers got stuck on an
    /// old verse.
    #[cfg_attr(test, mutants::skip)]
    pub(super) fn clear_lyrics_display(&self, playlist_id: i64) {
        let _ = self.ws_event_tx.send(ServerMsg::LyricsUpdate {
            playlist_id,
            line_en: None,
            line_sk: None,
            prev_line_en: None,
            next_line_en: None,
            active_word_index: None,
            word_count: None,
        });
        let _ = self
            .resolume_tx
            .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
        if let Some(client) = &self.presenter_client {
            let client = client.clone();
            tokio::spawn(async move {
                if let Err(e) = client
                    .push(crate::presenter::PresenterPayload::empty())
                    .await
                {
                    tracing::warn!(?e, "presenter clear on song-end failed (non-fatal)");
                }
            });
        }
    }
}
