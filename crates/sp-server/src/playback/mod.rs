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

/// Helper: fetch song, artist, gemini_failed for a video.
async fn get_video_title_info(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String, bool)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist, gemini_failed FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| {
        use sqlx::Row;
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        let gemini_failed: bool = r.get::<i32, _>("gemini_failed") != 0;
        (song, artist, gemini_failed)
    }))
}

/// Per-playlist pipeline state tracked by the engine.
struct PlaylistPipeline {
    pipeline: PlaybackPipeline,
    state: PlayState,
    mode: PlaybackMode,
    current_video_id: Option<i64>,
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
    /// Shared NDI backend — loaded once, shared across all pipeline threads.
    #[cfg(windows)]
    ndi_backend: Option<pipeline::SharedNdiBackend>,
    /// For sending text source updates to OBS.
    obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
    /// Used for title show/hide updates.
    #[allow(dead_code)]
    obs_event_tx: broadcast::Sender<ObsEvent>,
    /// For sending title show/hide commands to Resolume hosts.
    resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
}

impl PlaybackEngine {
    /// Create a new playback engine. Loads the NDI SDK once on Windows.
    pub fn new(
        pool: SqlitePool,
        obs_event_tx: broadcast::Sender<ObsEvent>,
        obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
        resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        #[cfg(windows)]
        let ndi_backend = {
            use sp_ndi::{NdiLib, RealNdiBackend};
            use std::sync::Arc;
            match NdiLib::load() {
                Ok(lib) => {
                    info!("NDI SDK loaded successfully for playback engine");
                    Some(Arc::new(RealNdiBackend::new(Arc::new(lib))))
                }
                Err(e) => {
                    warn!(%e, "NDI SDK not available — playback will not output NDI");
                    None
                }
            }
        };

        Self {
            pool,
            pipelines: HashMap::new(),
            event_rx,
            event_tx,
            #[cfg(windows)]
            ndi_backend,
            obs_cmd_tx,
            obs_event_tx,
            resolume_tx,
        }
    }

    /// Ensure a pipeline exists for the given playlist, creating one if needed.
    pub fn ensure_pipeline(&mut self, playlist_id: i64, ndi_name: &str) {
        let event_tx = self.event_tx.clone();

        #[cfg(windows)]
        let ndi_backend = self.ndi_backend.clone();
        #[cfg(not(windows))]
        let ndi_backend: Option<()> = None;

        self.pipelines.entry(playlist_id).or_insert_with(|| {
            info!(playlist_id, ndi_name, "creating playback pipeline");
            let pipeline =
                PlaybackPipeline::spawn(ndi_name.to_string(), ndi_backend, event_tx, playlist_id);
            PlaylistPipeline {
                pipeline,
                state: PlayState::Idle,
                mode: PlaybackMode::default(),
                current_video_id: None,
            }
        });
    }

    /// Receive the next pipeline event (for use in external select! loops).
    pub async fn recv_pipeline_event(&mut self) -> Option<(i64, PipelineEvent)> {
        self.event_rx.recv().await
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
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    if let Some(video_id) = pp.current_video_id {
                        // Title show after 1.5s
                        let pool = self.pool.clone();
                        let obs_cmd = self.obs_cmd_tx.clone();
                        let resolume_tx = self.resolume_tx.clone();
                        let pl_id = playlist_id;
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                            if let Ok(Some((song, artist, gemini_failed))) =
                                get_video_title_info(&pool, video_id).await
                            {
                                // OBS title
                                if let Some(cmd_tx) = obs_cmd {
                                    let text = if artist.is_empty() {
                                        song.clone()
                                    } else {
                                        format!("{song} - {artist}")
                                    };
                                    let source_name = sqlx::query_scalar::<_, String>(
                                        "SELECT obs_text_source FROM playlists WHERE id = ?",
                                    )
                                    .bind(pl_id)
                                    .fetch_optional(&pool)
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default();
                                    if !source_name.is_empty() {
                                        let _ = cmd_tx
                                            .send(crate::obs::ObsCommand::SetTextSource {
                                                source_name,
                                                text,
                                            })
                                            .await;
                                    }
                                }
                                // Resolume title
                                let _ = resolume_tx
                                    .send(crate::resolume::ResolumeCommand::ShowTitle {
                                        playlist_id: pl_id,
                                        song,
                                        artist,
                                        gemini_failed,
                                    })
                                    .await;
                            }
                        });

                        // Title hide 3.5s before end
                        let dur = *duration_ms;
                        if dur > 5000 {
                            let pool = self.pool.clone();
                            let obs_cmd = self.obs_cmd_tx.clone();
                            let resolume_tx = self.resolume_tx.clone();
                            let pl_id = playlist_id;
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(dur - 3500))
                                    .await;
                                // OBS clear
                                if let Some(cmd_tx) = obs_cmd {
                                    let source_name = sqlx::query_scalar::<_, String>(
                                        "SELECT obs_text_source FROM playlists WHERE id = ?",
                                    )
                                    .bind(pl_id)
                                    .fetch_optional(&pool)
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default();
                                    if !source_name.is_empty() {
                                        let _ = cmd_tx
                                            .send(crate::obs::ObsCommand::SetTextSource {
                                                source_name,
                                                text: String::new(),
                                            })
                                            .await;
                                    }
                                }
                                // Resolume hide
                                let _ = resolume_tx
                                    .send(crate::resolume::ResolumeCommand::HideTitle {
                                        playlist_id: pl_id,
                                    })
                                    .await;
                            });
                        }
                    }
                }
            }
            PipelineEvent::Position { .. } => {
                // Position events are tracked but title hide is now timer-based
                // (spawned in the Started handler above).
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
            let (resolume_tx, _) = mpsc::channel(16);
            let engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx);
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
            let (resolume_tx, _) = mpsc::channel(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx);

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
            let (resolume_tx, _) = mpsc::channel(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx);

            engine.ensure_pipeline(1, "NDI-1");
            engine.ensure_pipeline(2, "NDI-2");
            assert_eq!(engine.pipelines.len(), 2);
        });
    }
}
