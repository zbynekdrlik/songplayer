//! Manual /play dispatch + pause snapshot accessor for `PlaybackEngine`. #88.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap.
//!
//! Contract: `Pause` (via `PlayAction::Pause` in `execute_action`) captures
//! `(current_video_id, cached_position_ms)` into `PlaylistPipeline::paused_at`.
//! `handle_engine_play` (invoked from `lib.rs` on `EngineCommand::Play`)
//! reads the snapshot — if `Some`, resumes the same video at the recorded
//! position via `handle_play_video`, whose Play clears it (a failed lookup
//! keeps it, the pipeline stays paused); otherwise starts it with
//! `PlayEvent::Start` so fresh starts still pick a new video (#221 L4b: it
//! claims no program; off air it plays off program).
//! `handle_play_video` clears any stale `paused_at` so picking a different
//! setlist row after pause doesn't keep the old snapshot. `handle_play_video`
//! itself lives here too (moved out of `mod.rs` for the 1000-line cap, #215).

use std::sync::atomic::Ordering;

use sp_core::ws::ServerMsg;
use tracing::{info, warn};

use super::pipeline::PipelineCommand;
use super::state::{PlayEvent, PlayState};
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

        // #215: a pick inside a transition hold ends the hold (review rounds
        // 1 + 3): the song plays like any song played off program by hand,
        // and the clear below is not skipped as a held playlist's.
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.end_hold();
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

    /// Test-only: consume the paused snapshot for `playlist_id`; `None` if
    /// never paused. #88. (The ▶ only reads it since release 0.68.0 review
    /// round 6; `handle_play_video` clears it with its Play.)
    #[cfg(test)]
    pub(crate) fn take_paused_snapshot(&mut self, playlist_id: i64) -> Option<(i64, u64)> {
        self.pipelines
            .get_mut(&playlist_id)
            .and_then(|pp| pp.paused_at.take())
    }

    /// Manual /play: resume paused video if snapshot present, else start it. #88.
    /// A pipeline that is ALREADY Playing (e.g. an off-program dub after a page
    /// reload showed ▶ Prehrať) is a no-op. #221 L4b: a ▶ claims nothing (the
    /// playback authority decides what is on program): `PlayEvent::Start`
    /// leaves `scene_active` and the wall alone, so a playlist that is not on
    /// air plays OFF program (the Player reads "Hrá mimo programu"), and one
    /// on air starts as a scene-on would.
    pub async fn handle_engine_play(&mut self, playlist_id: i64) {
        // Read, not take: `handle_play_video` clears the snapshot with its
        // Play. A resume whose song lookup fails sends no Play, so the
        // pipeline stays paused and keeps its resume point (release 0.68.0
        // review round 6: an empty one let a queued `Started` through).
        let snapshot = self.pipelines.get(&playlist_id).and_then(|pp| pp.paused_at);
        match snapshot {
            Some((video_id, position_ms)) => {
                self.handle_play_video(playlist_id, video_id, Some(position_ms))
                    .await;
            }
            None => {
                let already_playing = self
                    .pipelines
                    .get(&playlist_id)
                    .is_some_and(|pp| !play_should_start(&pp.state));
                if already_playing {
                    tracing::debug!(playlist_id, "engine: /play on a playing pipeline — no-op");
                    return;
                }
                self.apply_event(playlist_id, PlayEvent::VideosAvailable)
                    .await;
                self.apply_event(playlist_id, PlayEvent::Start).await;
            }
        }
    }
}

/// Whether a manual /play with no pause snapshot may start the pipeline: only
/// when it is NOT already playing. Pure.
pub(super) fn play_should_start(state: &super::state::PlayState) -> bool {
    !matches!(state, super::state::PlayState::Playing { .. })
}
