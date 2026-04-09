//! Playback engine: state machine, pipeline management, and orchestration.
//!
//! The engine owns one [`PlaybackPipeline`] per active playlist and drives
//! transitions through the pure [`PlayState`] state machine.  Title timing
//! (show after 1.5 s, hide 3.5 s before end) is handled via Tokio timers.

pub mod pipeline;
pub mod state;

use std::collections::HashMap;

use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::obs::ObsEvent;
use crate::playlist::selector::VideoSelector;

use pipeline::{PipelineCommand, PipelineEvent, PlaybackPipeline};
use state::{PlayAction, PlayEvent, PlayState};

/// Per-playlist pipeline state tracked by the engine.
struct PlaylistPipeline {
    pipeline: PlaybackPipeline,
    state: PlayState,
    mode: PlaybackMode,
    current_video_id: Option<i64>,
    title_shown: bool,
}

/// Central playback orchestrator.
///
/// Owns pipelines for each active playlist, reacts to OBS scene changes and
/// pipeline events, and drives the [`PlayState`] state machine.
pub struct PlaybackEngine {
    pool: SqlitePool,
    pipelines: HashMap<i64, PlaylistPipeline>,
    event_rx: mpsc::UnboundedReceiver<(i64, PipelineEvent)>,
    event_tx: mpsc::UnboundedSender<(i64, PipelineEvent)>,
    /// Used for title show/hide updates (wired when title timing is complete).
    #[allow(dead_code)]
    obs_event_tx: broadcast::Sender<ObsEvent>,
}

impl PlaybackEngine {
    /// Create a new playback engine.
    pub fn new(pool: SqlitePool, obs_event_tx: broadcast::Sender<ObsEvent>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            pool,
            pipelines: HashMap::new(),
            event_rx,
            event_tx,
            obs_event_tx,
        }
    }

    /// Ensure a pipeline exists for the given playlist, creating one if needed.
    pub fn ensure_pipeline(&mut self, playlist_id: i64, ndi_name: &str) {
        self.pipelines.entry(playlist_id).or_insert_with(|| {
            info!(playlist_id, ndi_name, "creating playback pipeline");
            let pipeline =
                PlaybackPipeline::spawn(ndi_name.to_string(), self.event_tx.clone(), playlist_id);
            PlaylistPipeline {
                pipeline,
                state: PlayState::Idle,
                mode: PlaybackMode::default(),
                current_video_id: None,
                title_shown: false,
            }
        });
    }

    /// Handle a scene change from the OBS module.
    pub async fn handle_scene_change(&mut self, playlist_id: i64, on_program: bool) {
        let event = if on_program {
            PlayEvent::SceneOn
        } else {
            PlayEvent::SceneOff
        };
        self.apply_event(playlist_id, event).await;
    }

    /// Handle an event emitted by a pipeline thread.
    pub async fn handle_pipeline_event(&mut self, playlist_id: i64, event: PipelineEvent) {
        match &event {
            PipelineEvent::Started { duration_ms } => {
                debug!(playlist_id, duration_ms, "video started");
                // Title timing would be scheduled here via tokio::time::sleep.
            }
            PipelineEvent::Position {
                position_ms,
                duration_ms,
            } => {
                // Check if we should hide the title (3500ms before end).
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    if pp.title_shown && *duration_ms > 3500 && *position_ms > duration_ms - 3500 {
                        pp.title_shown = false;
                        debug!(playlist_id, "hiding title (near end)");
                    }
                }
            }
            PipelineEvent::Ended => {
                self.apply_event(playlist_id, PlayEvent::VideoEnded).await;
            }
            PipelineEvent::Error(msg) => {
                warn!(playlist_id, %msg, "pipeline error");
                self.apply_event(playlist_id, PlayEvent::VideoError(msg.clone()))
                    .await;
            }
        }
    }

    /// Handle a user command (skip, mode change, etc.).
    pub async fn handle_command(&mut self, playlist_id: i64, cmd: PlayEvent) {
        // If it's a mode change, update the stored mode.
        if let PlayEvent::SetMode(new_mode) = &cmd {
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.mode = *new_mode;
            }
        }
        self.apply_event(playlist_id, cmd).await;
    }

    /// Run the engine event loop until shutdown.
    pub async fn run(mut self, mut shutdown: broadcast::Receiver<()>) {
        info!("playback engine started");

        loop {
            tokio::select! {
                Some((playlist_id, event)) = self.event_rx.recv() => {
                    self.handle_pipeline_event(playlist_id, event).await;
                }
                _ = shutdown.recv() => {
                    info!("playback engine shutting down");
                    break;
                }
            }
        }

        // Drop all pipelines (sends Shutdown to each thread).
        self.pipelines.clear();
        info!("playback engine stopped");
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Apply a play event to the state machine and execute the resulting action.
    async fn apply_event(&mut self, playlist_id: i64, event: PlayEvent) {
        let Some(pp) = self.pipelines.get_mut(&playlist_id) else {
            warn!(playlist_id, "no pipeline for playlist");
            return;
        };

        let mode = pp.mode;
        let old_state = pp.state.clone();
        let (new_state, action) = old_state.transition(event, mode);
        pp.state = new_state;

        if let Some(action) = action {
            self.execute_action(playlist_id, action).await;
        }
    }

    /// Execute a [`PlayAction`] produced by the state machine.
    async fn execute_action(&mut self, playlist_id: i64, action: PlayAction) {
        match action {
            PlayAction::SelectAndPlay => {
                let mode = self
                    .pipelines
                    .get(&playlist_id)
                    .map(|pp| pp.mode)
                    .unwrap_or_default();
                let current = self
                    .pipelines
                    .get(&playlist_id)
                    .and_then(|pp| pp.current_video_id);

                match VideoSelector::select_next(&self.pool, playlist_id, mode, current).await {
                    Ok(Some(video_id)) => {
                        debug!(playlist_id, video_id, "selected video");
                        // Look up the file path from DB.
                        match crate::db::models::get_video_file_path(&self.pool, video_id).await {
                            Ok(Some(file_path)) => {
                                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                                    pp.current_video_id = Some(video_id);
                                    pp.state = PlayState::Playing { video_id };
                                    pp.title_shown = false;
                                    info!(playlist_id, video_id, %file_path, "sent Play command");
                                    pp.pipeline.send(PipelineCommand::Play(file_path.into()));

                                    // Record play in history.
                                    if let Err(e) = crate::db::models::record_play(
                                        &self.pool,
                                        playlist_id,
                                        video_id,
                                    )
                                    .await
                                    {
                                        warn!(playlist_id, video_id, %e, "failed to record play");
                                    }
                                }
                            }
                            Ok(None) => {
                                warn!(
                                    playlist_id,
                                    video_id, "video has no file_path (not normalized?)"
                                );
                            }
                            Err(e) => {
                                warn!(playlist_id, video_id, %e, "failed to get video file_path");
                            }
                        }
                    }
                    Ok(None) => {
                        debug!(playlist_id, "no videos available for selection");
                    }
                    Err(e) => {
                        warn!(playlist_id, %e, "video selection failed");
                    }
                }
            }

            PlayAction::ReplayCurrent => {
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    if let Some(video_id) = pp.current_video_id {
                        debug!(playlist_id, "replaying current video");
                        match crate::db::models::get_video_file_path(&self.pool, video_id).await {
                            Ok(Some(file_path)) => {
                                pp.pipeline.send(PipelineCommand::Play(file_path.into()));
                            }
                            Ok(None) => {
                                warn!(playlist_id, video_id, "no file_path for replay");
                            }
                            Err(e) => {
                                warn!(playlist_id, video_id, %e, "failed to get file_path for replay");
                            }
                        }
                    }
                }
            }

            PlayAction::Pause => {
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    pp.pipeline.send(PipelineCommand::Pause);
                    debug!(playlist_id, "paused pipeline");
                }
            }

            PlayAction::SendBlack => {
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    pp.pipeline.send(PipelineCommand::Stop);
                    debug!(playlist_id, "sent black / stopped pipeline");
                }
            }

            PlayAction::Stop => {
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    pp.pipeline.send(PipelineCommand::Stop);
                    debug!(playlist_id, "stopped pipeline");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_construction() {
        // Verify the engine can be constructed without panicking.
        // We use a fake pool — the engine doesn't touch the DB at construction.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            let (obs_tx, _obs_rx) = broadcast::channel(16);
            let engine = PlaybackEngine::new(pool, obs_tx);
            assert!(engine.pipelines.is_empty());
        });
    }

    #[test]
    fn engine_ensure_pipeline_creates_entry() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            let (obs_tx, _obs_rx) = broadcast::channel(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx);

            engine.ensure_pipeline(1, "TestNDI");
            assert!(engine.pipelines.contains_key(&1));

            // Calling again should not create a second pipeline.
            engine.ensure_pipeline(1, "TestNDI");
            assert_eq!(engine.pipelines.len(), 1);
        });
    }

    #[test]
    fn engine_ensure_pipeline_multiple_playlists() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            let (obs_tx, _obs_rx) = broadcast::channel(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx);

            engine.ensure_pipeline(1, "NDI-1");
            engine.ensure_pipeline(2, "NDI-2");
            assert_eq!(engine.pipelines.len(), 2);
        });
    }
}
