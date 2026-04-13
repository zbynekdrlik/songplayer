//! Playback engine: state machine, pipeline management, and orchestration.
//!
//! The engine owns one [`PlaybackPipeline`] per active playlist and drives
//! transitions through the pure [`PlayState`] state machine.  Title timing
//! (show after 1.5 s, hide 3.5 s before end) is handled via Tokio timers.

pub mod pipeline;
pub mod state;
pub mod submitter;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
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

/// Maximum number of past videos tracked per playlist for the Previous
/// button. Bounded to keep memory O(1) per playlist — older entries are
/// dropped from the front when the capacity is exceeded. 50 is plenty
/// for human navigation.
const PREVIOUS_HISTORY_CAPACITY: usize = 50;

/// Pure predicate: may we send a `NowPlaying` update given the elapsed
/// milliseconds since the last one for this playlist?
///
/// Extracted so it can be unit-tested at exact boundary values (499 /
/// 500 / 501). Testing the parent method against real `Instant::now()`
/// under coverage tooling is racy; testing this pure function is not.
#[inline]
fn should_send_position_update(elapsed_ms: u64) -> bool {
    elapsed_ms >= POSITION_BROADCAST_INTERVAL_MS
}

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
    /// Whether the OBS program scene currently shows this playlist's NDI
    /// output. Updated from `handle_scene_change`. Used by
    /// `on_video_processed` to decide whether a freshly-normalized video
    /// should auto-start playback. Purely engine-level bookkeeping — the
    /// pure [`PlayState`] state machine does not track it.
    scene_active: bool,
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
    /// Stack of previously-played `video_id`s, most recent last. Pushed
    /// when a new video is selected (via `SelectAndPlay`); popped by
    /// `handle_previous`. Bounded to [`PREVIOUS_HISTORY_CAPACITY`].
    history: VecDeque<i64>,
    /// Active lyrics state for karaoke display. Loaded when a video with
    /// lyrics starts; cleared when the video ends.
    lyrics_state: Option<crate::lyrics::renderer::LyricsState>,
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
    cache_dir: PathBuf,
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
        cache_dir: PathBuf,
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
            cache_dir,
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
                scene_active: false,
                title_show_abort: None,
                title_hide_abort: None,
                cached_song: String::new(),
                cached_artist: String::new(),
                cached_duration_ms: 0,
                last_now_playing_broadcast: None,
                history: VecDeque::with_capacity(PREVIOUS_HISTORY_CAPACITY),
                lyrics_state: None,
            }
        });
    }

    /// Receive the next pipeline event (for use in external select! loops).
    pub async fn recv_pipeline_event(&mut self) -> Option<(i64, PipelineEvent)> {
        self.event_rx.recv().await
    }

    /// Handle a scene change from the OBS module.
    ///
    /// When the scene goes on program we first fire `VideosAvailable` to
    /// lift the pipeline out of `Idle` (the pure state machine's
    /// `Idle + SceneOn` transition is a no-op by design), then fire
    /// `SceneOn` which runs `SelectAndPlay`. Folding both into one
    /// entry point guarantees every caller (OBS bridge, API, tests)
    /// goes through the same sequence.
    pub async fn handle_scene_change(&mut self, playlist_id: i64, on_program: bool) {
        // Engine-level bookkeeping: remember which pipelines currently
        // own the program scene, independent of the pure state machine.
        // `on_video_processed` reads this to decide whether a
        // freshly-normalized video should auto-start playback.
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.scene_active = on_program;
        }

        if on_program {
            self.apply_event(playlist_id, PlayEvent::VideosAvailable)
                .await;
            self.apply_event(playlist_id, PlayEvent::SceneOn).await;
        } else {
            self.apply_event(playlist_id, PlayEvent::SceneOff).await;
        }
    }

    /// Re-wake pipelines parked in `WaitingForScene` after the download
    /// worker finishes normalizing a video.
    ///
    /// The stuck-WaitingForScene bug (shipped in 0.11.0): on first boot
    /// after the V4 migration reset every row to `normalized = 0`, OBS
    /// is already sitting on an `sp-*` scene when SongPlayer starts.
    /// The scene-on event fires `SelectAndPlay`, which finds nothing
    /// (DB is empty), so the pipeline parks in `WaitingForScene` with
    /// `current_video_id = None`. When the download worker finally
    /// produces a normalized video there is nothing to re-run the
    /// selection — the engine is deaf to the download worker.
    ///
    /// This method is that missing listener. The `youtube_id` argument
    /// is taken from the `processed:{id}` broadcast that
    /// [`crate::downloader::DownloadWorker`] emits on every completed
    /// pipeline run. We look up the playlist that owns this video id;
    /// if that playlist's scene is currently active and no video is
    /// playing, we re-run `SelectAndPlay` through the state machine.
    pub async fn on_video_processed(&mut self, youtube_id: &str) {
        // Find the playlist that owns this just-processed video.
        let row = match sqlx::query("SELECT playlist_id FROM videos WHERE youtube_id = ?")
            .bind(youtube_id)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                debug!(
                    youtube_id,
                    "on_video_processed: no video row for youtube_id, ignoring"
                );
                return;
            }
            Err(e) => {
                warn!(youtube_id, %e, "on_video_processed: DB lookup failed");
                return;
            }
        };

        use sqlx::Row;
        let playlist_id: i64 = row.get("playlist_id");

        // Only re-wake if the pipeline is waiting AND its scene is
        // currently on program AND nothing is playing. Otherwise a
        // processed event could steal the current video.
        let should_wake = self
            .pipelines
            .get(&playlist_id)
            .map(|pp| {
                matches!(pp.state, PlayState::WaitingForScene)
                    && pp.scene_active
                    && pp.current_video_id.is_none()
            })
            .unwrap_or(false);

        if !should_wake {
            debug!(
                playlist_id,
                youtube_id, "on_video_processed: pipeline not in wake-eligible state, ignoring"
            );
            return;
        }

        info!(
            playlist_id,
            youtube_id, "on_video_processed: re-running SelectAndPlay on previously-stuck pipeline"
        );

        // Re-fire SceneOn through the state machine. `WaitingForScene
        // + SceneOn` transitions to `WaitingForScene + SelectAndPlay`,
        // which now has a normalized video to pick.
        self.apply_event(playlist_id, PlayEvent::SceneOn).await;
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
                        match load_lyrics_for_video(&pool, &cache_dir, video_id).await {
                            Ok(Some(track)) => {
                                pp.lyrics_state =
                                    Some(crate::lyrics::renderer::LyricsState::new(track));
                                debug!(playlist_id, video_id, "lyrics loaded for karaoke");
                            }
                            Ok(None) => {
                                pp.lyrics_state = None;
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
            PipelineEvent::Position {
                position_ms,
                duration_ms,
            } => {
                // Throttled re-broadcast of NowPlaying with the updated
                // position. Title hide is timer-based (spawned in the
                // Started handler above) so no position-driven hide work
                // happens here.
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

    /// Handle the Previous-track command: pop the most recent entry from
    /// the per-playlist history stack and send the pipeline a `Play`
    /// command for that video.
    ///
    /// If the history is empty (fresh startup or too many Previous
    /// presses), the command is a no-op. Calling `Previous` does NOT
    /// re-push the current video, so pressing it repeatedly walks
    /// backwards through the stack one step at a time.
    #[cfg_attr(test, mutants::skip)]
    pub async fn handle_previous(&mut self, playlist_id: i64) {
        let prev_video_id = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp.history.pop_back(),
            None => {
                warn!(playlist_id, "Previous: no pipeline for playlist");
                return;
            }
        };

        let Some(video_id) = prev_video_id else {
            debug!(playlist_id, "Previous: history empty, ignoring");
            return;
        };

        match crate::db::models::get_song_paths(&self.pool, video_id).await {
            Ok(Some((video_path, audio_path))) => {
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.current_video_id = Some(video_id);
                    pp.state = PlayState::Playing { video_id };
                    info!(
                        playlist_id,
                        video_id, %video_path, %audio_path,
                        "Previous → replaying song from history"
                    );
                    pp.pipeline.send(PipelineCommand::Play {
                        video: video_path.into(),
                        audio: audio_path.into(),
                    });

                    // Broadcast the state change so the dashboard updates.
                    let _ = self.ws_event_tx.send(ServerMsg::PlaybackStateChanged {
                        playlist_id,
                        state: WsPlaybackState::Playing,
                        mode: pp.mode,
                    });
                }
            }
            Ok(None) => {
                warn!(
                    playlist_id,
                    video_id, "Previous: history entry has no paths"
                );
            }
            Err(e) => {
                warn!(
                    playlist_id,
                    video_id, %e, "Previous: failed to get paths"
                );
            }
        }
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
    /// Send empty lyrics to dashboard and Resolume to clear stale display.
    fn clear_lyrics_display(&self, playlist_id: i64) {
        let _ = self.ws_event_tx.send(ServerMsg::LyricsUpdate {
            playlist_id,
            line_en: None,
            line_sk: None,
            prev_line_en: None,
            next_line_en: None,
            active_word_index: None,
            word_count: None,
        });
        let _ = self
            .resolume_tx
            .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
    }

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
            // Resolume subtitle update
            let (en, sk) = lyrics.resolume_lines(position_ms);
            match en {
                Some(en_text) => {
                    let _ = self.resolume_tx.try_send(
                        crate::resolume::ResolumeCommand::ShowSubtitles { en: en_text, sk },
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
                        match crate::db::models::get_song_paths(&self.pool, video_id).await {
                            Ok(Some((video_path, audio_path))) => {
                                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                                    // Push the previous video to the
                                    // per-playlist history stack before
                                    // overwriting `current_video_id`.
                                    if let Some(prev) = pp.current_video_id {
                                        pp.history.push_back(prev);
                                        while pp.history.len() > PREVIOUS_HISTORY_CAPACITY {
                                            pp.history.pop_front();
                                        }
                                    }
                                    pp.current_video_id = Some(video_id);
                                    pp.state = PlayState::Playing { video_id };
                                    info!(
                                        playlist_id, video_id,
                                        %video_path, %audio_path,
                                        "sent Play command"
                                    );
                                    pp.pipeline.send(PipelineCommand::Play {
                                        video: video_path.into(),
                                        audio: audio_path.into(),
                                    });

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
                                    video_id, "video has no sidecar paths (not normalized?)"
                                );
                            }
                            Err(e) => {
                                warn!(playlist_id, video_id, %e, "failed to get song paths");
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
                        match crate::db::models::get_song_paths(&self.pool, video_id).await {
                            Ok(Some((video_path, audio_path))) => {
                                pp.pipeline.send(PipelineCommand::Play {
                                    video: video_path.into(),
                                    audio: audio_path.into(),
                                });
                            }
                            Ok(None) => {
                                warn!(playlist_id, video_id, "no song paths for replay");
                            }
                            Err(e) => {
                                warn!(playlist_id, video_id, %e, "failed to get song paths for replay");
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

/// Load lyrics JSON for a video from the cache directory, if available.
#[cfg_attr(test, mutants::skip)]
async fn load_lyrics_for_video(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_id: i64,
) -> Result<Option<sp_core::lyrics::LyricsTrack>, anyhow::Error> {
    use sqlx::Row;
    let row = sqlx::query("SELECT youtube_id, has_lyrics FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;
    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };
    let has_lyrics: i64 = row.get("has_lyrics");
    if has_lyrics == 0 {
        return Ok(None);
    }
    let youtube_id: String = row.get("youtube_id");
    let lyrics_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    if !lyrics_path.exists() {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&lyrics_path).await?;
    let track: sp_core::lyrics::LyricsTrack = serde_json::from_str(&content)?;
    Ok(Some(track))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
