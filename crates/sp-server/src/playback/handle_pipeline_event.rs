//! `PlaybackEngine::handle_pipeline_event` — extracted to keep
//! `playback/mod.rs` under the 1000-line airuleset cap.
//!
//! This is the top-level orchestration entry point that dispatches on
//! `PipelineEvent` variants emitted by per-playlist pipeline threads.
//! See the doc comment on `handle_pipeline_event` for details.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, info, warn};

use super::pipeline::PipelineEvent;
use super::state::PlayEvent;
use super::title::{self, OBS_TITLE_SOURCE};
use super::{PlaybackEngine, lyrics_loader};

impl PlaybackEngine {
    /// Handle an event emitted by a pipeline thread.
    ///
    /// This is the top-level orchestration entry point — it dispatches on
    /// pipeline events and spawns title-show / title-hide timer tasks. Unit
    /// testing it requires a full DB + OBS + Resolume harness; the
    /// individual concerns (timer cancellation, title formatting, get_video_title_info)
    /// have dedicated unit tests below.
    #[cfg_attr(test, mutants::skip)]
    pub async fn handle_pipeline_event(&mut self, playlist_id: i64, event: PipelineEvent) {
        match &event {
            PipelineEvent::Started { duration_ms } => {
                // 1) Broadcast NowPlaying to the dashboard first so it
                //    switches from "Nothing playing" immediately.
                self.broadcast_now_playing_on_start(playlist_id, *duration_ms)
                    .await;

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
                                pp.lyrics_state = Some(
                                    crate::lyrics::renderer::LyricsState::with_lead_and_offset(
                                        track, lead_ms, offset_ms,
                                    ),
                                );
                                info!(
                                    playlist_id,
                                    video_id,
                                    lines = line_count,
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

                // 2) Cancel any pending title timers from a previous video on
                //    this playlist. Without this, a stale hide_title from a
                //    skipped 4-min song would fire 3.5s before that song's
                //    natural end during the next song, clearing the title
                //    mid-playback.
                let video_id_opt = if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.current_video_id
                } else {
                    None
                };

                if let Some(video_id) = video_id_opt {
                    // Title show after 1.5s — scene_active read at FIRE time.
                    let scene_active = self
                        .pipelines
                        .get(&playlist_id)
                        .map(|pp| pp.scene_active.clone())
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

                    let pool = self.pool.clone();
                    let obs_cmd = self.obs_cmd_tx.clone();
                    let resolume_tx = self.resolume_tx.clone();
                    let pl_id = playlist_id;

                    let show_handle = tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        if !scene_active.load(Ordering::Acquire) {
                            debug!(playlist_id = pl_id, "title suppressed — off program");
                            return;
                        }
                        if title::push_title(&pool, obs_cmd.as_ref(), &resolume_tx, video_id).await
                        {
                            info!(playlist_id = pl_id, video_id, "title shown");
                        }
                    });

                    // Store the show abort handle.
                    if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                        pp.title_show_abort = Some(show_handle.abort_handle());
                    }

                    // Title hide 3.5s before end (only if duration is known and long enough).
                    if dur > 5000 {
                        let obs_cmd = self.obs_cmd_tx.clone();
                        let resolume_tx = self.resolume_tx.clone();
                        let pl_id = playlist_id;
                        let hide_at = dur - 3500;
                        let hide_handle = tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(hide_at)).await;

                            if let Some(cmd_tx) = obs_cmd {
                                let _ = cmd_tx
                                    .send(crate::obs::ObsCommand::SetTextSource {
                                        source_name: OBS_TITLE_SOURCE.to_string(),
                                        text: String::new(),
                                    })
                                    .await;
                            }

                            let _ = resolume_tx
                                .send(crate::resolume::ResolumeCommand::HideTitle)
                                .await;

                            debug!(playlist_id = pl_id, "title hidden");
                        });

                        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                            pp.title_hide_abort = Some(hide_handle.abort_handle());
                        }
                    }
                }
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
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cached_position_ms = *position_ms;
                }
                self.dispatch_lyrics_if_changed(playlist_id, *position_ms);
                self.maybe_broadcast_position_update(playlist_id, *position_ms, *duration_ms);
            }
            PipelineEvent::Ended => {
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.lyrics_state = None;
                }
                self.clear_lyrics_display(playlist_id);
                self.apply_event(playlist_id, PlayEvent::VideoEnded).await;
            }
            PipelineEvent::Error(msg) => {
                warn!(playlist_id, %msg, "pipeline error");
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.lyrics_state = None;
                }
                self.clear_lyrics_display(playlist_id);
                self.apply_event(playlist_id, PlayEvent::VideoError(msg.clone()))
                    .await;
            }
            ev @ PipelineEvent::HealthSnapshot { .. } => {
                self.handle_health_snapshot(playlist_id, ev.clone());
            }
        }
    }
}
