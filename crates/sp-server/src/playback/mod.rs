//! Playback engine: state machine, pipeline management, and orchestration.
//!
//! The engine owns one [`PlaybackPipeline`] per active playlist and drives
//! transitions through the pure [`PlayState`] state machine.  Title timing
//! (show after 1.5 s, hide 3.5 s before end) is handled via Tokio timers.

pub mod pipeline;
pub mod state;
pub mod submitter;

use std::collections::HashMap;
use std::time::Instant;

use sp_core::playback::{PlaybackMode, PlaybackState as WsPlaybackState};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::obs::ObsEvent;
use crate::playlist::selector::VideoSelector;

use pipeline::{PipelineCommand, PipelineEvent, PlaybackPipeline};
use state::{PlayAction, PlayEvent, PlayState};

/// OBS text source name used for the fallback title display (in the
/// CG OVERLAY scene). Must match the source name in OBS exactly.
const OBS_TITLE_SOURCE: &str = "#sp-title";

/// Minimum gap between `NowPlaying` position re-broadcasts per playlist.
/// Keeps the WebSocket from flooding the dashboard on high-frequency
/// `PipelineEvent::Position` events.
const POSITION_BROADCAST_INTERVAL_MS: u64 = 500;

/// Map the internal server-side [`PlayState`] to the wire-level
/// [`sp_core::playback::PlaybackState`] used by the dashboard.
fn play_state_to_ws(state: &PlayState) -> WsPlaybackState {
    match state {
        PlayState::Idle => WsPlaybackState::Idle,
        PlayState::WaitingForScene => WsPlaybackState::WaitingForScene,
        PlayState::Playing { .. } => WsPlaybackState::Playing,
    }
}

/// Helper: fetch song, artist for a video.
async fn get_video_title_info(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| {
        use sqlx::Row;
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        (song, artist)
    }))
}

/// Per-playlist pipeline state tracked by the engine.
struct PlaylistPipeline {
    pipeline: PlaybackPipeline,
    state: PlayState,
    mode: PlaybackMode,
    current_video_id: Option<i64>,
    /// Abort handle for the in-flight title-show timer (1.5s after Started).
    /// Cancelled when a new video starts so a stale timer from the previous
    /// video can't fire mid-song after a skip.
    title_show_abort: Option<tokio::task::AbortHandle>,
    /// Abort handle for the in-flight title-hide timer (3.5s before end).
    title_hide_abort: Option<tokio::task::AbortHandle>,
    /// Cached song/artist/duration from the current video, so `Position`
    /// events can re-broadcast `NowPlaying` without re-querying the DB
    /// on every update.
    cached_song: String,
    cached_artist: String,
    cached_duration_ms: u64,
    /// Timestamp of the last `NowPlaying` broadcast — used to throttle
    /// position updates to `POSITION_BROADCAST_INTERVAL_MS`.
    last_now_playing_broadcast: Option<Instant>,
}

impl PlaylistPipeline {
    /// Cancel any pending title timers (called before spawning new ones on
    /// each `Started` event).
    fn cancel_title_timers(&mut self) {
        if let Some(h) = self.title_show_abort.take() {
            h.abort();
        }
        if let Some(h) = self.title_hide_abort.take() {
            h.abort();
        }
    }
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
    /// WebSocket broadcast — forwards `NowPlaying` and `PlaybackStateChanged`
    /// messages to the dashboard.
    ws_event_tx: broadcast::Sender<ServerMsg>,
}

impl PlaybackEngine {
    /// Create a new playback engine. Loads the NDI SDK once on Windows.
    pub fn new(
        pool: SqlitePool,
        obs_event_tx: broadcast::Sender<ObsEvent>,
        obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
        resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
        ws_event_tx: broadcast::Sender<ServerMsg>,
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
            ws_event_tx,
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
                title_show_abort: None,
                title_hide_abort: None,
                cached_song: String::new(),
                cached_artist: String::new(),
                cached_duration_ms: 0,
                last_now_playing_broadcast: None,
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
    ///
    /// This is the top-level orchestration entry point — it dispatches on
    /// pipeline events and spawns title-show / title-hide timer tasks. Unit
    /// testing it requires a full DB + OBS + Resolume harness; the
    /// individual concerns (timer cancellation, title formatting, get_video_title_info)
    /// have dedicated unit tests below.
    #[cfg_attr(test, mutants::skip)]
    pub async fn handle_pipeline_event(&mut self, playlist_id: i64, event: PipelineEvent) {
        // Fetch + broadcast NowPlaying / position updates BEFORE the existing
        // timer logic so the dashboard stays fresh.
        match &event {
            PipelineEvent::Started { duration_ms } => {
                self.broadcast_now_playing_on_start(playlist_id, *duration_ms)
                    .await;
            }
            PipelineEvent::Position {
                position_ms,
                duration_ms,
            } => {
                self.maybe_broadcast_position_update(playlist_id, *position_ms, *duration_ms);
            }
            PipelineEvent::Ended | PipelineEvent::Error(_) => {}
        }

        match &event {
            PipelineEvent::Started { duration_ms } => {
                debug!(playlist_id, duration_ms, "video started");
                let dur = *duration_ms;

                // Cancel any pending title timers from a previous video on this
                // playlist. Without this, a stale hide_title from a skipped 4-min
                // song would fire 3.5s before that song's natural end during the
                // next song, clearing the title mid-playback.
                let video_id_opt = if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                    pp.current_video_id
                } else {
                    None
                };

                if let Some(video_id) = video_id_opt {
                    // Title show after 1.5s.
                    let pool = self.pool.clone();
                    let obs_cmd = self.obs_cmd_tx.clone();
                    let resolume_tx = self.resolume_tx.clone();
                    let pl_id = playlist_id;

                    let show_handle = tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        if let Ok(Some((song, artist))) =
                            get_video_title_info(&pool, video_id).await
                        {
                            // Format the displayed text once for OBS.
                            let text = if artist.is_empty() {
                                song.clone()
                            } else if song.is_empty() {
                                artist.clone()
                            } else {
                                format!("{song} - {artist}")
                            };

                            // OBS fallback (single hardcoded source name).
                            if let Some(cmd_tx) = obs_cmd {
                                let _ = cmd_tx
                                    .send(crate::obs::ObsCommand::SetTextSource {
                                        source_name: OBS_TITLE_SOURCE.to_string(),
                                        text,
                                    })
                                    .await;
                            }

                            // Resolume — registry broadcasts to all hosts; driver targets all #sp-title clips.
                            let _ = resolume_tx
                                .send(crate::resolume::ResolumeCommand::ShowTitle { song, artist })
                                .await;

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
            PipelineEvent::Position { .. } => {
                // Position events are tracked but title hide is now timer-based
                // (spawned in the Started handler above).
            }
            PipelineEvent::Ended => {
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                }
                self.apply_event(playlist_id, PlayEvent::VideoEnded).await;
            }
            PipelineEvent::Error(msg) => {
                warn!(playlist_id, %msg, "pipeline error");
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cancel_title_timers();
                }
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
        let (new_state, action) = old_state.clone().transition(event, mode);
        pp.state = new_state.clone();

        if let Some(action) = action {
            self.execute_action(playlist_id, action).await;
        }

        // After the action (which may itself mutate the state to Playing),
        // broadcast the final state if it differs from the pre-transition state.
        let final_state = self
            .pipelines
            .get(&playlist_id)
            .map(|pp| pp.state.clone())
            .unwrap_or(new_state);
        if old_state != final_state {
            let _ = self.ws_event_tx.send(ServerMsg::PlaybackStateChanged {
                playlist_id,
                state: play_state_to_ws(&final_state),
                mode,
            });
        }
    }

    /// Cache the video's song/artist/duration and broadcast `NowPlaying`
    /// with `position_ms: 0`. Called when a pipeline reports a `Started`
    /// event (i.e. playback just began).
    async fn broadcast_now_playing_on_start(&mut self, playlist_id: i64, duration_ms: u64) {
        let video_id = match self
            .pipelines
            .get(&playlist_id)
            .and_then(|pp| pp.current_video_id)
        {
            Some(id) => id,
            None => return,
        };

        let (song, artist) = match get_video_title_info(&self.pool, video_id).await {
            Ok(Some(pair)) => pair,
            _ => (String::new(), String::new()),
        };

        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.cached_song = song.clone();
            pp.cached_artist = artist.clone();
            pp.cached_duration_ms = duration_ms;
            pp.last_now_playing_broadcast = Some(Instant::now());
        }

        let _ = self.ws_event_tx.send(ServerMsg::NowPlaying {
            playlist_id,
            video_id,
            song,
            artist,
            position_ms: 0,
            duration_ms,
        });
    }

    /// Throttle and re-broadcast `NowPlaying` with an updated `position_ms`.
    ///
    /// Skips the broadcast if less than
    /// [`POSITION_BROADCAST_INTERVAL_MS`] has elapsed since the last
    /// broadcast for the same playlist.
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
            Some(t) => now.duration_since(t).as_millis() as u64 >= POSITION_BROADCAST_INTERVAL_MS,
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

    /// Execute a [`PlayAction`] produced by the state machine.
    ///
    /// Top-level orchestration that touches the DB, video selector, and
    /// pipeline thread. Tested via integration / live verification on
    /// win-resolume rather than unit-mutation tests.
    #[cfg_attr(test, mutants::skip)]
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
            let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
            let engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
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
            let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);

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
            let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
            let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);

            engine.ensure_pipeline(1, "NDI-1");
            engine.ensure_pipeline(2, "NDI-2");
            assert_eq!(engine.pipelines.len(), 2);
        });
    }

    /// Regression test for stale title-timer bug: when a video is skipped,
    /// the previous video's title-show/hide timers must be cancelled so they
    /// don't fire mid-song during the next video.
    #[test]
    fn cancel_title_timers_aborts_pending_handles() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // Spawn a long-running task and grab its abort handle.
            let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                let _ = started_tx.send(());
                // This sleep represents the 1.5s/N seconds title timer.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "should not reach here"
            });
            // Wait for the task to actually start.
            started_rx.await.unwrap();

            let mut pp = PlaylistPipeline {
                pipeline: PlaybackPipeline::spawn(
                    "test".to_string(),
                    None,
                    mpsc::unbounded_channel().0,
                    1,
                ),
                state: PlayState::Idle,
                mode: PlaybackMode::default(),
                current_video_id: None,
                title_show_abort: Some(task.abort_handle()),
                title_hide_abort: None,
                cached_song: String::new(),
                cached_artist: String::new(),
                cached_duration_ms: 0,
                last_now_playing_broadcast: None,
            };

            assert!(pp.title_show_abort.is_some());
            pp.cancel_title_timers();
            assert!(pp.title_show_abort.is_none());

            // Verify the underlying task was actually aborted.
            let result = task.await;
            assert!(
                result.is_err(),
                "task should have been aborted, got: {result:?}"
            );
            assert!(result.unwrap_err().is_cancelled());
        });
    }

    /// Verify get_video_title_info returns the actual song+artist from the DB.
    /// Kills mutants that replace the function body with constants.
    #[tokio::test]
    async fn get_video_title_info_returns_song_and_artist() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, song, artist) VALUES (42, 1, 'abc', 'My Song', 'Artist Name')")
            .execute(&pool)
            .await
            .unwrap();

        let result = get_video_title_info(&pool, 42).await.unwrap();
        assert_eq!(
            result,
            Some(("My Song".to_string(), "Artist Name".to_string()))
        );
    }

    #[tokio::test]
    async fn get_video_title_info_returns_none_for_missing_video() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let result = get_video_title_info(&pool, 999).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn get_video_title_info_handles_null_song_and_artist() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id) VALUES (42, 1, 'abc')")
            .execute(&pool)
            .await
            .unwrap();
        let result = get_video_title_info(&pool, 42).await.unwrap();
        assert_eq!(result, Some((String::new(), String::new())));
    }

    /// Regression for issue #9: a pipeline `Started` event must produce a
    /// `ServerMsg::NowPlaying` broadcast with song/artist/duration pulled
    /// from the DB. Before the fix, the engine had no `ws_event_tx` and
    /// nothing ever reached the dashboard.
    #[tokio::test]
    async fn pipeline_started_event_broadcasts_now_playing() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
             VALUES (1, 'P', 'url', 'SP-p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
             VALUES (42, 1, 'abc123', 'Test Song', 'Test Artist')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (obs_tx, _) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(16);
        let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
        engine.ensure_pipeline(1, "SP-p");

        // Simulate a video having been selected (so current_video_id is set).
        if let Some(pp) = engine.pipelines.get_mut(&1) {
            pp.current_video_id = Some(42);
        }

        engine
            .handle_pipeline_event(
                1,
                PipelineEvent::Started {
                    duration_ms: 180_000,
                },
            )
            .await;

        // The first message on ws_rx should be our NowPlaying.
        // (PlaybackStateChanged may follow, but NowPlaying must be present.)
        let mut saw_now_playing = false;
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_millis(500), ws_rx.recv()).await {
                Ok(Ok(ServerMsg::NowPlaying {
                    playlist_id,
                    video_id,
                    song,
                    artist,
                    position_ms,
                    duration_ms,
                })) => {
                    assert_eq!(playlist_id, 1);
                    assert_eq!(video_id, 42);
                    assert_eq!(song, "Test Song");
                    assert_eq!(artist, "Test Artist");
                    assert_eq!(position_ms, 0);
                    assert_eq!(duration_ms, 180_000);
                    saw_now_playing = true;
                    break;
                }
                Ok(Ok(_other)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(saw_now_playing, "expected a NowPlaying broadcast");
    }

    /// Fast-firing `Position` events must not flood the broadcast channel:
    /// only one `NowPlaying` should be sent per `POSITION_BROADCAST_INTERVAL_MS`.
    #[tokio::test(start_paused = true)]
    async fn position_events_are_throttled() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
             VALUES (1, 'P', 'url', 'SP-p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
             VALUES (42, 1, 'abc123', 'Song', 'Artist')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (obs_tx, _) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, mut ws_rx) = broadcast::channel::<ServerMsg>(64);
        let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
        engine.ensure_pipeline(1, "SP-p");
        if let Some(pp) = engine.pipelines.get_mut(&1) {
            pp.current_video_id = Some(42);
        }

        engine
            .handle_pipeline_event(
                1,
                PipelineEvent::Started {
                    duration_ms: 180_000,
                },
            )
            .await;

        // Drain messages produced by Started (NowPlaying + possibly PlaybackStateChanged).
        while ws_rx.try_recv().is_ok() {}

        // Fire 10 Position events in quick succession.
        for i in 1..=10u64 {
            engine
                .handle_pipeline_event(
                    1,
                    PipelineEvent::Position {
                        position_ms: i * 10,
                        duration_ms: 180_000,
                    },
                )
                .await;
        }

        // With throttling (500ms interval) and virtual time NOT yet advanced,
        // the only broadcast that should have been produced is the initial
        // one on Started (already drained). Zero additional NowPlaying.
        assert!(
            ws_rx.try_recv().is_err(),
            "no NowPlaying should leak while within the 500ms throttle window"
        );

        // Advance virtual time 600ms and fire once more — should produce one message.
        tokio::time::advance(std::time::Duration::from_millis(600)).await;
        engine
            .handle_pipeline_event(
                1,
                PipelineEvent::Position {
                    position_ms: 700,
                    duration_ms: 180_000,
                },
            )
            .await;

        match ws_rx.try_recv() {
            Ok(ServerMsg::NowPlaying { position_ms, .. }) => {
                assert_eq!(position_ms, 700);
            }
            other => panic!("expected NowPlaying after throttle window, got {other:?}"),
        }
    }
}
