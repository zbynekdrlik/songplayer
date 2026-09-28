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
//! setlist row after pause doesn't keep the old snapshot. `handle_play_video`
//! itself lives here too (moved out of `mod.rs` for the 1000-line cap, #215).

use std::sync::atomic::Ordering;

use sp_core::ws::ServerMsg;
use tracing::{info, warn};

use super::pipeline::PipelineCommand;
use super::state::PlayState;
use super::transport_state::transport_from_play_state;
use super::{PlaybackEngine, play_state_to_ws};

impl PlaybackEngine {
    /// Jump to a specific video within a playlist and start playing it.
    ///
    /// For custom playlists this also updates `playlists.current_position` so
    /// the next `Skip` advances to position+1. For youtube playlists the
    /// column is ignored by the selector — only the pipeline command is
    /// relevant. The previously-playing video (if any) is pushed onto the
    /// history stack so `Previous` still walks the history.
    // mutants::skip: I/O-heavy orchestrator — covered by handle_play_video integration tests in playback/tests.rs.
    #[cfg_attr(test, mutants::skip)]
    pub async fn handle_play_video(
        &mut self,
        playlist_id: i64,
        video_id: i64,
        position_ms: Option<u64>,
    ) {
        // Resolve paths first — if the video row is unknown, no side-effects.
        let paths = match crate::db::models::get_song_paths(&self.pool, video_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                warn!(
                    playlist_id,
                    video_id, "PlayVideo: no paths for video; ignoring"
                );
                return;
            }
            Err(e) => {
                warn!(playlist_id, video_id, %e, "PlayVideo: DB lookup failed; ignoring");
                return;
            }
        };

        // For custom playlists, bump current_position to the clicked item's
        // position so Skip continues from the right place.
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or_default();
        if kind.as_deref() == Some("custom") {
            if let Ok(Some(pos)) =
                crate::db::models::position_for_playlist_item(&self.pool, playlist_id, video_id)
                    .await
            {
                let _ = sqlx::query("UPDATE playlists SET current_position = ? WHERE id = ?")
                    .bind(pos)
                    .bind(playlist_id)
                    .execute(&self.pool)
                    .await;
            }
        }

        // Clear Resolume `#sp-subs` and Presenter immediately so the previous
        // song's last line doesn't linger during the new song's intro
        // (e.g. song 17 has ~19s before first lyric).
        self.clear_lyrics_display(playlist_id);

        // Send the pipeline command and update engine bookkeeping.
        let (video_path, audio_path) = paths;
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            if let Some(prev) = pp.current_video_id {
                if prev != video_id {
                    pp.history.push_back(prev);
                }
            }
            pp.current_video_id = Some(video_id);
            pp.last_presenter_text = None;
            pp.last_resolume_subtitles_signature = None;
            pp.last_lyrics_ws_signature = None;
            pp.paused_at = None;
            pp.state = PlayState::Playing { video_id };
            // #215: a pick inside a transition hold ends the hold: the song
            // plays like any song played off program by hand (review round 1).
            pp.end_hold();
            info!(
                playlist_id,
                video_id, %video_path, %audio_path,
                position_ms,
                "PlayVideo → jumping to clicked song"
            );
            pp.begin_play(position_ms.unwrap_or(0)); // its Started fixes the title clock
            pp.pipeline.send(PipelineCommand::Play {
                video: video_path.into(),
                audio: audio_path.into(),
                start_position_ms: position_ms,
            });

            // #134: a manually-picked song must count toward "already
            // played" the same as a naturally-selected one (SelectAndPlay,
            // above, does this same call) — otherwise the unplayed-first
            // selector would immediately re-offer a song the operator just
            // played by hand.
            if let Err(e) = crate::db::models::record_play(&self.pool, playlist_id, video_id).await
            {
                warn!(playlist_id, video_id, %e, "PlayVideo: failed to record play");
            }

            // #170: gate on scene_active so a PlayVideo on an off-program
            // playlist shows WaitingForScene, matching the health-label replay.
            // #201: transport reports the raw decoding state (Playing here) so
            // an off-program dub reads `⏸ Pauza` while it plays.
            let _ = self.ws_event_tx.send(ServerMsg::PlaybackStateChanged {
                playlist_id,
                state: play_state_to_ws(
                    &PlayState::Playing { video_id },
                    pp.scene_active.load(Ordering::Acquire),
                ),
                mode: pp.mode,
                transport: transport_from_play_state(&PlayState::Playing { video_id }),
            });
        } else {
            warn!(playlist_id, video_id, "PlayVideo: no pipeline for playlist");
        }
        self.resync_after_play(playlist_id).await;
    }

    /// Consume paused snapshot for `playlist_id`; `None` if never paused. #88.
    pub fn take_paused_snapshot(&mut self, playlist_id: i64) -> Option<(i64, u64)> {
        self.pipelines
            .get_mut(&playlist_id)
            .and_then(|pp| pp.paused_at.take())
    }

    /// Manual /play: resume paused video if snapshot present, else scene-on. #88.
    /// A pipeline that is ALREADY Playing (e.g. an off-program dub after a page
    /// reload showed ▶ Prehrať) is a no-op — the scene-on fallback would flag an
    /// off-program output as on program and re-push its title to the wall.
    pub async fn handle_engine_play(&mut self, playlist_id: i64) {
        match self.take_paused_snapshot(playlist_id) {
            Some((video_id, position_ms)) => {
                self.handle_play_video(playlist_id, video_id, Some(position_ms))
                    .await;
            }
            None => {
                let already_playing = self
                    .pipelines
                    .get(&playlist_id)
                    .is_some_and(|pp| !play_should_scene_on(&pp.state));
                if already_playing {
                    tracing::debug!(playlist_id, "engine: /play on a playing pipeline — no-op");
                    return;
                }
                self.handle_scene_change(playlist_id, true).await;
            }
        }
    }
}

/// Whether a manual /play with no pause snapshot may fall through to the
/// scene-on dispatch: only when the pipeline is NOT already playing. Pure.
pub(super) fn play_should_scene_on(state: &super::state::PlayState) -> bool {
    !matches!(state, super::state::PlayState::Playing { .. })
}
