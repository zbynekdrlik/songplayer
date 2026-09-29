//! `PlaybackEngine::handle_pipeline_event` — extracted to keep
//! `playback/mod.rs` under the 1000-line airuleset cap.
//!
//! This is the top-level orchestration entry point that dispatches on
//! `PipelineEvent` variants emitted by per-playlist pipeline threads.
//! See the doc comment on `handle_pipeline_event` for details.

use tracing::{debug, info, warn};

use super::pipeline::PipelineEvent;
use super::state::PlayEvent;
use super::title;
use super::{PlaybackEngine, lyrics_loader};

impl PlaybackEngine {
    /// Handle an event emitted by a pipeline thread.
    ///
    /// This is the top-level orchestration entry point — it dispatches on
    /// pipeline events and spawns title-show / title-hide timer tasks. Its
    /// branches are pinned by behaviour tests on an in-memory DB
    /// (`tests_hold.rs`, `tests_scene_change.rs`, `tests_play_video.rs`,
    /// `program_authority_tests.rs`); the
    /// individual concerns (timer cancellation, title formatting,
    /// get_video_title_info) have dedicated unit tests.
    #[cfg_attr(test, mutants::skip)]
    pub async fn handle_pipeline_event(&mut self, playlist_id: i64, event: PipelineEvent) {
        match &event {
            PipelineEvent::Started { duration_ms } => {
                // 1) Broadcast NowPlaying to the dashboard first so it
                //    switches from "Nothing playing" immediately.
                self.broadcast_now_playing_on_start(playlist_id, *duration_ms)
                    .await;

                // A Play that a pause overtook (the #215 hold's end, the
                // dashboard's Pause): the song is loaded, but paused. None of
                // it reaches the wall, the stage display or the title timers
                // (release 0.68.0 blockers, review round 1); its resume's
                // `Started` does all of that. "Paused since the last Play" is
                // `paused_at` (every Pause sets it, every Play clears it), not
                // the state: a failed selection leaves WaitingForScene with
                // the song still playing (review round 5).
                if self
                    .pipelines
                    .get(&playlist_id)
                    .is_some_and(|pp| pp.paused_at.is_some())
                {
                    debug!(playlist_id, "started, but paused — nothing to show");
                    return;
                }

                // Load lyrics for karaoke display
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    if let Some(video_id) = pp.current_video_id {
                        let cache_dir = self.cache_dir.clone();
                        let pool = self.pool.clone();
                        match lyrics_loader::load_lyrics_for_video(&pool, &cache_dir, video_id)
                            .await
                        {
                            Ok(Some((track, offset_ms))) => {
                                let lead_ms = lyrics_loader::load_lyrics_lead_ms(&pool).await;
                                let line_count = track.lines.len();
                                let source = track.source.clone();
                                let pipeline_version = track.version;
                                let state =
                                    crate::lyrics::renderer::LyricsState::with_lead_and_offset(
                                        track, lead_ms, offset_ms,
                                    );
                                // #217: how many wall lines the fragments merged into,
                                // and whether the wall leads (song) or not (speech).
                                let display_lines = state.display_plan().lines().len();
                                let display_profile = state.display_plan().profile();
                                pp.lyrics_state = Some(state);
                                info!(
                                    playlist_id,
                                    video_id,
                                    lines = line_count,
                                    display_lines,
                                    ?display_profile,
                                    source = %source,
                                    pipeline_version,
                                    lead_ms,
                                    offset_ms,
                                    "lyrics: loaded"
                                );
                            }
                            Ok(None) => {
                                pp.lyrics_state = None;
                                info!(
                                    playlist_id,
                                    video_id,
                                    "lyrics: no track available — wall will show no subtitles"
                                );
                                self.clear_lyrics_display(playlist_id);
                            }
                            Err(e) => {
                                warn!(playlist_id, video_id, "failed to load lyrics: {e}");
                                pp.lyrics_state = None;
                                self.clear_lyrics_display(playlist_id);
                            }
                        }
                    }
                }

                debug!(playlist_id, duration_ms, "video started");
                let dur = *duration_ms;

                // 2) Fix this song's title clock and arm its timers from it:
                //    show 1.5 s from now, hide 3.5 s before the end. A recovery
                //    or a scene-on reads the same clock (#217 addendum 3).
                //    Arming first cancels any pending timer of a previous
                //    video on this playlist: a stale hide_title from a skipped
                //    4-min song would fire 3.5s before that song's natural end
                //    during the next song, clearing the title mid-playback.
                //    A resume hides 3.5 s before the song's real end.
                let now = tokio::time::Instant::now();
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    let start_ms = pp.play_start_ms;
                    pp.title_clock = pp
                        .current_video_id
                        .map(|video_id| title::TitleClock::new(video_id, now, dur, start_ms));
                }
                self.arm_title_timers(playlist_id, now);
            }
            PipelineEvent::Position {
                position_ms,
                duration_ms,
            } => {
                // Lyrics fast path — no throttle. Fires Resolume ShowSubtitles,
                // Presenter push, and karaoke WebSocket when the current/next
                // line differs from the per-pipeline snapshot. Bounded by one
                // Position event tick (~33 ms).
                //
                // Throttled NowPlaying rebroadcast for the dashboard progress
                // bar. Title hide is timer-based (spawned in the Started
                // handler above) so no position-driven hide work happens here.
                // A report before the new song's `Started` (a Play clears the
                // clock) is the OLD song's: `Position` names no video, and it
                // must not move the new song's pause point (review round 4).
                if let Some(pp) = self.pipelines.get_mut(&playlist_id)
                    && pp.title_clock.is_some()
                {
                    pp.cached_position_ms = *position_ms;
                }
                self.dispatch_lyrics_if_changed(playlist_id, *position_ms);
                self.maybe_broadcast_position_update(playlist_id, *position_ms, *duration_ms);
            }
            PipelineEvent::Ended => {
                // #215: held off program, it pauses: no song starts there, and
                // the shared subtitle clips belong to the playlist on program.
                if self.pause_if_held(playlist_id, "its song ended").await {
                    return;
                }
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.lyrics_state = None;
                }
                self.clear_lyrics_display(playlist_id);
                self.apply_event(playlist_id, PlayEvent::VideoEnded).await;
            }
            PipelineEvent::Error(msg) => {
                warn!(playlist_id, %msg, "pipeline error");
                if self.pause_if_held(playlist_id, "its song failed").await {
                    return;
                }
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.lyrics_state = None;
                }
                self.clear_lyrics_display(playlist_id);
                self.apply_event(playlist_id, PlayEvent::VideoError(msg.clone()))
                    .await;
            }
            PipelineEvent::SceneOffDue(due) => self.scene_off_due(playlist_id, *due).await,
            PipelineEvent::OnProgram(on) => self.on_program(playlist_id, *on).await, // #221 L4b
            ev @ PipelineEvent::HealthSnapshot { .. } => {
                self.handle_health_snapshot(playlist_id, ev.clone());
                // #198 item 5: the sync handler only QUEUES a changed receiver
                // count; drain and persist it here in the async context (in
                // order, one write per changed output), replacing the per-poll
                // detached `tokio::spawn` that could panic for a sync caller.
                for (pid, connections) in self.ndi_health_registry.drain_pending_persists() {
                    if let Err(e) =
                        crate::db::models_ndi::set_last_receiver_count(&self.pool, pid, connections)
                            .await
                    {
                        tracing::debug!(playlist_id = pid, %e, "ndi: failed to persist receiver count");
                    }
                }
            }
        }
    }
}
