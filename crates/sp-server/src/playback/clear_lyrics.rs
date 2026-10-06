//! `clear_lyrics_display` — extracted from `playback/mod.rs` to keep that
//! file under the 1000-line cap. It follows the dispatch gates (a held
//! playlist clears nothing, an off-program one leaves the shared subtitle
//! clips alone; release 0.68.0 blockers; a playlist that does not own the
//! wall clears neither the clips nor the Presenter, release 0.69.0 🟡 2).

use std::sync::atomic::Ordering;

use sp_core::ws::ServerMsg;

impl super::PlaybackEngine {
    /// Send empty lyrics to dashboard, Resolume AND Presenter to clear
    /// stale display when the previous song ends or the operator switches
    /// to a song without lyrics. Without the Presenter clear, the stage
    /// display kept showing the last line of the previous song until the
    /// next song's first line pushed — cue for singers got stuck on an
    /// old verse.
    ///
    /// It follows `dispatch_lyrics_if_changed`'s gates (release 0.68.0
    /// blockers, review round 1): a playlist HELD off program through a #215
    /// transition clears nothing, and one off program leaves the shared
    /// subtitle clips alone. Its song end, a song without lyrics or a
    /// PlayVideo blanked the on-program playlist's `#sp-subs` line.
    ///
    /// #221 (release 0.69.0 review 🟡 2): only the wall owner's clear reaches
    /// the shared subtitle clips and the Presenter
    /// (`OnAirPlaylists::may_write_wall`); with no owner, nobody's.
    #[cfg_attr(test, mutants::skip)]
    pub(super) fn clear_lyrics_display(&self, playlist_id: i64) {
        let owns_wall = self.on_air.may_write_wall(playlist_id);
        let (on_program, held) = self
            .pipelines
            .get(&playlist_id)
            .map_or((true, false), |pp| {
                (
                    pp.scene_active.load(Ordering::Acquire),
                    pp.scene_off_due.is_some(),
                )
            });
        if held {
            return;
        }
        let _ = self.ws_event_tx.send(ServerMsg::LyricsUpdate {
            playlist_id,
            line_en: None,
            line_sk: None,
            prev_line_en: None,
            next_line_en: None,
            active_word_index: None,
            word_count: None,
        });
        if !owns_wall {
            return;
        }
        if on_program {
            let _ = self
                .resolume_tx
                .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
        }
        crate::presenter::push_empty(self.presenter_client.as_ref(), "song end");
    }
}
