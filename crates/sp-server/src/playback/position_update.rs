//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Pure delegate — same method, same behavior, accessed via `PlaybackEngine::maybe_broadcast_position_update`.

impl super::PlaybackEngine {
    fn maybe_broadcast_position_update(
        &mut self,
        playlist_id: i64,
        position_ms: u64,
        duration_ms: u64,
    ) {
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };

        let now = Instant::now();
        let should_send = match pp.last_now_playing_broadcast {
            Some(t) => should_send_position_update(now.duration_since(t).as_millis() as u64),
            None => true,
        };
        if !should_send {
            return;
        }
        pp.last_now_playing_broadcast = Some(now);

        let video_id = match pp.current_video_id {
            Some(id) => id,
            None => return,
        };
        let song = pp.cached_song.clone();
        let artist = pp.cached_artist.clone();
        let dur = if duration_ms > 0 {
            duration_ms
        } else {
            pp.cached_duration_ms
        };

        let _ = self.ws_event_tx.send(ServerMsg::NowPlaying {
            playlist_id,
            video_id,
            song,
            artist,
            position_ms,
            duration_ms: dur,
        });

        // Emit lyrics update for karaoke display
        if let Some(ref lyrics) = pp.lyrics_state {
            let msg = lyrics.update(playlist_id, position_ms);
            let _ = self.ws_event_tx.send(msg);
            // Resolume subs gated on scene_active to prevent off-program
            // playlists clobbering `#sp-subs` (2026-04-19 event).
            if pp.scene_active.load(Ordering::Acquire) {
                match lyrics.resolume_lines_with_next(position_ms) {
                    Some((en, next_en, sk, next_sk)) => {
                        let _ = self.resolume_tx.try_send(
                            crate::resolume::ResolumeCommand::ShowSubtitles {
                                en,
                                next_en,
                                sk,
                                next_sk,
                                suppress_en: pp.cached_suppress_en,
                            },
                        );
                    }
                    None => {
                        let _ = self
                            .resolume_tx
                            .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
                    }
                }
            }
        }

        // Presenter stage-display push: fire-and-forget on line change.
        if let Some(lyrics) = &pp.lyrics_state
            && let Some((cur, nxt)) = lyrics.presenter_lines(position_ms)
        {
            pp.last_presenter_text = crate::presenter::maybe_push_line(
                self.presenter_client.as_ref(),
                pp.last_presenter_text.take(),
                cur,
                nxt,
                &pp.cached_song,
                &pp.cached_artist,
            );
        }
    }
}
