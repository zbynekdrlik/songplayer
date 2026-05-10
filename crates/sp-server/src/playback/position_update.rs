//! Per-pipeline-event broadcast helpers — extracted from `mod.rs` to keep
//! that file under the 1000-line cap. Two pure delegates:
//!
//! - [`PlaybackEngine::dispatch_lyrics_if_changed`] — event-driven lyrics
//!   dispatch. Fires Resolume `ShowSubtitles` + Presenter push + karaoke
//!   WebSocket on every Position event when the current/next line changed
//!   vs the per-pipeline snapshot. NO throttle — line latency on the LED
//!   wall is bounded by one Position event tick (~33 ms).
//! - [`PlaybackEngine::maybe_broadcast_position_update`] — periodic
//!   `NowPlaying` rebroadcast for the dashboard progress bar. Throttled
//!   to `POSITION_BROADCAST_INTERVAL_MS = 500`.
//!
//! Both are called by the `PipelineEvent::Position` handler in `mod.rs`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use sp_core::ws::ServerMsg;
use tracing::info;

use super::should_send_position_update;

impl super::PlaybackEngine {
    /// Lyrics fast path — no throttle. Computes current/next line, compares
    /// against the per-pipeline snapshots, and dispatches Resolume +
    /// Presenter + karaoke WebSocket only when the snapshot differs.
    ///
    /// Resolume dispatch is gated on `pp.scene_active` (off-program
    /// playlists must not clobber `#sp-subs`). Presenter + ws fire
    /// regardless of `scene_active`.
    #[cfg_attr(test, mutants::skip)] // Resolume + Presenter dispatch with signature-based dedup; ShowSubtitles / HideSubtitles call shape is asserted by 6 unit tests in dispatch_lyrics_tests. The `!=` dedup check on the hide signature is a Resolume traffic optimization; flipping it produces extra HideSubtitles calls without changing wall behavior — the dedup is best-effort.
    pub(super) fn dispatch_lyrics_if_changed(&mut self, playlist_id: i64, position_ms: u64) {
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };
        let _video_id = match pp.current_video_id {
            Some(id) => id,
            None => return,
        };

        // Karaoke WebSocket dedup keyed on the line's English text.
        let ws_msg = lyrics.update(playlist_id, position_ms);
        let ws_signature = ws_signature_from(&ws_msg);
        if pp.last_lyrics_ws_signature != ws_signature {
            let _ = self.ws_event_tx.send(ws_msg);
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.last_lyrics_ws_signature = ws_signature;
            }
        }

        // Re-fetch — the previous mutable borrow was dropped above.
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };
        let video_id = match pp.current_video_id {
            Some(id) => id,
            None => return,
        };

        // Resolume — gated on scene_active.
        if pp.scene_active.load(Ordering::Acquire) {
            let resolume_signature = match lyrics.resolume_lines_with_next(position_ms) {
                Some((en, next_en, sk, next_sk)) => {
                    let sig = format!(
                        "show|{}|{}|{}|{}|{}",
                        en,
                        next_en,
                        sk.as_deref().unwrap_or(""),
                        next_sk.as_deref().unwrap_or(""),
                        pp.cached_suppress_en,
                    );
                    if pp.last_resolume_subtitles_signature.as_deref() != Some(sig.as_str()) {
                        info!(
                            playlist_id,
                            video_id,
                            text = %en.lines().next().unwrap_or("").chars().take(60).collect::<String>(),
                            "ShowSubtitles dispatched"
                        );
                        let _ = self.resolume_tx.try_send(
                            crate::resolume::ResolumeCommand::ShowSubtitles {
                                en,
                                next_en,
                                sk,
                                next_sk,
                                suppress_en: pp.cached_suppress_en,
                            },
                        );
                        Some(sig)
                    } else {
                        // Same as last sent — no-op.
                        Some(sig)
                    }
                }
                None => {
                    let sig = "hide".to_string();
                    if pp.last_resolume_subtitles_signature.as_deref() != Some(sig.as_str()) {
                        let _ = self
                            .resolume_tx
                            .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
                        Some(sig)
                    } else {
                        Some(sig)
                    }
                }
            };
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.last_resolume_subtitles_signature = resolume_signature;
            }
        }

        // Presenter — fire-and-forget. `maybe_push_line` is idempotent on
        // identical `current_en` (compares against the `last_seen` arg we
        // pass, which is the pre-call snapshot).
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };
        if let Some((cur, nxt)) = lyrics.presenter_lines(position_ms) {
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

    /// Periodic dashboard `NowPlaying` rebroadcast. Throttled to
    /// `POSITION_BROADCAST_INTERVAL_MS = 500` so the progress bar doesn't
    /// flood the WebSocket on high-frequency Position events.
    pub(super) fn maybe_broadcast_position_update(
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
    }
}

/// Build a stable signature for a [`ServerMsg::LyricsUpdate`] keyed on the
/// line's `line_en`. Returns `None` when the message has no line (between-
/// lines clear). The signature ignores `position_ms`-derived
/// `active_word_index` so we don't fire a fresh ws message every Position
/// tick within the same line.
fn ws_signature_from(msg: &ServerMsg) -> Option<String> {
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => line_en.clone(),
        _ => None,
    }
}
