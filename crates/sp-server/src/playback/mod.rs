//! Playback engine: state machine, pipeline management, and orchestration.
//!
//! The engine owns one [`PlaybackPipeline`] per active playlist and drives
//! transitions through the pure [`PlayState`] state machine.  Title timing
//! (show after 1.5 s, hide 3.5 s before end) is handled via Tokio timers.

pub mod asio_format; // #233: an ASIO driver's sample types + the L/R channel fill (pure)
pub mod asrc; // #233: the ASIO output's resampler (rubato Async sinc) + the re-centre splice
pub mod asrc_servo; // #233: the ASIO output's drift servo (camera-box's asrc-compensator, for an output), pure
pub mod audio_grid;
pub mod audio_out; // #233: the program audio's fan-out to its outputs (one queue + thread each)
pub mod audio_out_block; // #233: one program boundary's audio block for the outputs
pub mod audio_out_config; // #233: the outputs' settings — strict PATCH parse, lenient stored read
pub mod audio_out_migrate; // #233: vban_* → the output list, once (the old keys stay)
pub mod audio_out_task; // #233: the outputs' settings task (keep / build / stop, DNS, migration)
pub mod band_pool; // #223: the SP-program sender's persistent row-band workers (no thread per picture)
mod clear_lyrics;
pub mod clock_health;
pub mod dashboard_replay; // #225: the engine's last dashboard state per playlist, replayed on WS connect
pub mod decode_thread; // #223 S0: the one way a decode thread starts (producer + decode bench)
mod engine_play;
pub(crate) mod failure_backoff; // #229: the pause after failed opens (pure; the selector's pick too)
mod failure_retry; // #229: the engine's retry of a playlist whose opens fail
pub mod fleet_shift; // #224 part 2: a date step relabels (pure split + the relabel registry)
pub mod frame_buf; // #203 shared-frame seam: Arc<Vec<u8>> holdover, no pixel copy
mod handle_pipeline_event;
pub mod lock_state;
pub mod loop_stats; // the per-minute `pipeline: loop-stats` line + the shared percentile rule (pure)
mod lyrics_loader;
mod mix; // #184 round G set_mix (impl PlaybackEngine, 1000-line cap split)
pub mod mmcss; // #210 part 2: a real-time sender thread as an MMCSS "Pro Audio" thread
pub mod ndi_health;
pub(crate) mod ndi_health_expect; // #221: SP-program's receiver (the program's degraded reason + log)
mod ndi_health_transport; // #201 round 2: pure reported-label -> TransportState (Linux-tested)
pub mod ndi_input; // #212: the NDI input "OBS manuál" on the genlock grid → the program bus
pub mod nv12_fit; // #215: aspect-kept NV12 placement (preview letterbox + program fit)
pub mod paced_grid; // #147 the paced output's own boundary clock (pure, Linux-tested)
pub mod paced_output; // #168/#147 a playlist's paced output: handoff + consumer → the program bus (cross-platform)
pub mod pacer;
pub mod pacer_queue; // #147 producer/consumer: pure bounded look-ahead frame queue
pub mod pacer_sink; // #203 pacer scheduling + shared-frame standby submit helpers
pub mod pacer_spin; // #147 the boundary wait's spin: yields once the wall stands still (pure, Linux-tested)
pub mod pacing_stats; // #147 PacingStats (split out of ndi_health.rs, 1000-line cap)
pub mod pipeline;
#[cfg(windows)]
pub(crate) mod pipeline_paced;
#[cfg(windows)]
pub(crate) mod pipeline_paced_idle;
#[cfg(windows)]
pub(crate) mod pipeline_paced_submit; // #168 the paced heartbeat over the output's snapshot
#[cfg(not(windows))]
pub(crate) mod pipeline_stub;
mod playlist_mode; // #225 unit 2: a mode the playlist's row holds — applied + told
mod position_update;
pub mod preview; // #15 part 2: live low-res video preview tap
pub mod proc_mem; // #147 r9: SongPlayer's own page faults/min + working set on the paced loop-stats line
mod program_authority; // #221 L4b: SP-program's playlist drives playback
pub mod program_bus; // #209: the program bus (SongPlayer = master switcher, NDI SP-program)
pub mod program_canvas; // #223: SP-program's ONE picture layout (FHD) + the fit into it
pub mod program_max; // #223 S2: SP-program-MAX hand-off, setting + telemetry
pub mod program_max_worker; // #223 S2: the program-max thread (GPU compose + Spout send)
pub mod program_on_air; // #221: what is on air (the bus's watch value) + the one scene-name resolver
pub mod program_output; // #209: the SP-program sender + its thread
pub mod program_output_timing; // #210: the sender's per-boundary timing window (pure, health.timing)
pub mod program_switch; // #221: the ONE switch path of a scene press (catalog, cut, manual forward)
pub mod program_transition; // #215: transition window + crossfade math (pure, Linux-tested)
pub mod program_transition_settings; // #221 L5: the transition settings → the bus's spec (task)
pub(crate) mod recovery; // + the RecoveryEvent → engine forwarder lib.rs spawns
mod runtime_pipeline;
pub mod scene_catalog; // #221: which scene is a playlist's, from its NDI output name (no cg OBS lookup)
mod scene_off; // #215: the deferred scene-go-off pause of the program's outgoing source
mod seek; // #217: the engine's seek (the song's title clock follows it)
pub mod startup_pipelines; // the startup pipelines (id order, row mode) + SP-program's #196 port wait
pub mod stat_window; // #210 part 2: shared pure two-bucket worst + WARN rate limit
pub mod state;
pub mod submit_handoff; // #168 output-side split: pure emit->submit handoff decisions
pub mod submitter;
mod test_helpers;
mod title;
mod title_timers; // #217 addendum 3: title timers armed from the song's TitleClock
mod transport_state; // #201 pure PlayState->TransportState mapping (Linux-tested)
pub mod vban_clock; // #224 part 2: VBAN's + the NDI input's wall clock, VBAN's date-step slew
pub mod vban_out; // #210: the program's VBAN audio output (queue, paced thread, socket, stats)
pub mod vban_packet; // #210: the pure VBAN packet encoder (header, INT24, 8×200 split)
pub mod vban_rate; // #233: a VBAN destination's fixed-ratio rate conversion (rubato Fft)
pub mod vban_stall; // #210 part 2: the VBAN thread's late packets (ring, window max, WARN)
pub mod video_decode; // #223 S3b: `video_hw_decode` (read at each song open) + its status
pub mod wallclock;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use chrono::Utc;
use sp_core::playback::{PlaybackMode, PlaybackState as WsPlaybackState};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::obs::ObsEvent;
use crate::playlist::selector::VideoSelector;

use pipeline::{PipelineCommand, PipelineEvent, PlaybackPipeline};
use state::{PlayAction, PlayEvent, PlayState};

/// The loaded NDI SDK, shared (Windows only): `SP-program`'s sender
/// (`program_output.rs`) and the NDI input "OBS manuál" use it.
#[cfg(windows)]
pub type SharedNdiBackend = Arc<sp_ndi::RealNdiBackend>;

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

/// Map the internal server-side [`PlayState`] to the wire-level dashboard
/// [`WsPlaybackState`]. #170: a pipeline the engine holds as `Playing` but
/// whose scene is OFF program (a hold, or #221 L4b a ▶ off air; the Player
/// tells both from a paused pipeline by the transport) must map to
/// `WaitingForScene`, as `handle_health_snapshot` reconciles the health label
/// `(Playing, Playing, scene_active = false) → Paused`. A live
/// `Playing` for such a pipeline flips a paused selector row to Playing, the
/// selector re-orders it to the top, and a click races the moving row.
fn play_state_to_ws(state: &PlayState, scene_active: bool) -> WsPlaybackState {
    match state {
        PlayState::Idle => WsPlaybackState::Idle,
        PlayState::WaitingForScene => WsPlaybackState::WaitingForScene,
        // #170: Playing but scene off program == paused (dark wall) -> the
        // dashboard's "waiting for scene" (the WS replay re-tells it, #225).
        PlayState::Playing { .. } if !scene_active => WsPlaybackState::WaitingForScene,
        PlayState::Playing { .. } => WsPlaybackState::Playing,
    }
}

/// Per-playlist pipeline state tracked by the engine.
struct PlaylistPipeline {
    pipeline: PlaybackPipeline,
    state: PlayState,
    mode: PlaybackMode,
    current_video_id: Option<i64>,
    /// On program: on air per the playback authority (#221 L4b). `Arc<AtomicBool>`
    /// so the detached title-show task (1.5s delay) reads the CURRENT
    /// value at fire time, not a stale snapshot from spawn time.
    scene_active: Arc<AtomicBool>,
    /// Abort handle for the title-show timer (1.5s after Started). Cancelled
    /// on new video so a stale timer from a skipped prior song can't fire.
    title_show_abort: Option<tokio::task::AbortHandle>,
    /// Abort handle for the title-hide timer (3.5s before end).
    title_hide_abort: Option<tokio::task::AbortHandle>,
    /// #215: the pending re-check (`SceneOffDue`: its id, its sleeping task)
    /// of a playlist HELD off program through a transition (`scene_off.rs`);
    /// `Some` = held. Its pause, a scene back on program, an operator's pick
    /// and a newer hold end it (`end_hold`). A held playlist has no side
    /// effects: no lyrics line goes out, and its song's end, a failure or a
    /// skip pause it instead of starting a song off program (release 0.68.0
    /// blockers, design record 5863318980).
    scene_off_due: Option<(u64, tokio::task::AbortHandle)>,
    /// Cached song/artist/duration so `Position` events can re-broadcast
    /// `NowPlaying` without re-querying the DB.
    cached_song: String,
    cached_artist: String,
    cached_duration_ms: u64,
    /// v0.22.0: skip EN Resolume when true (baked-in video lyrics).
    cached_suppress_en: bool,
    /// #142: song carries Claude's verified "reference" lyrics — the
    /// renderer appends " ★" to every displayed line on the LED wall.
    cached_lyrics_reference: bool,
    /// Timestamp of the last `NowPlaying` broadcast — used to throttle
    /// position updates to `POSITION_BROADCAST_INTERVAL_MS`.
    last_now_playing_broadcast: Option<Instant>,
    /// Stack of previously-played `video_id`s, most recent last. Pushed
    /// when a new video is selected (via `SelectAndPlay`); popped by
    /// `handle_previous`. Bounded to [`PREVIOUS_HISTORY_CAPACITY`].
    history: VecDeque<i64>,
    /// Active lyrics state for karaoke display. Loaded when a video with
    /// lyrics starts; cleared by every Play (`begin_play`), when the video
    /// ends and when the pipeline pauses. A scene-off keeps it through the
    /// #215 hold (design record 5863318980).
    lyrics_state: Option<crate::lyrics::renderer::LyricsState>,
    /// Presenter-push debounce: the lines last sent (#222).
    last_presenter_text: Option<crate::presenter::PushedLine>,
    /// Last Resolume ShowSubtitles signature; dedup key for `dispatch_lyrics_if_changed`. Reset on song change.
    last_resolume_subtitles_signature: Option<String>,
    /// Last karaoke ws line text; dedup key for `dispatch_lyrics_if_changed`. Reset on song change.
    last_lyrics_ws_signature: Option<String>,
    /// Last reported playback position (ms): the Play's start (`begin_play`,
    /// 0 or a resume's position), then every Position event once the song's
    /// `Started` fixed its title clock (an earlier one is the old song's,
    /// release 0.68.0 review round 4). Read by the Pause snapshot and by
    /// handle_resolume_recovery to re-push the current subtitle line, which
    /// may be up to one Position tick (~500 ms) behind the actual playhead.
    cached_position_ms: u64,
    /// The title window of the song whose `Started` the engine last handled:
    /// the instants the title timers sleep until, and that a recovery or a
    /// scene-on reads (#217 addendum 3). Every Play clears it (`begin_play`),
    /// so it is `None` from a song change to the new `Started`.
    title_clock: Option<title::TitleClock>,
    /// Where the current Play ASKED to start: 0, or a resume's position.
    /// The title clock counts from where `Started` says the song really
    /// starts (#217: 0 when the start seek failed); this is only logged next
    /// to it.
    play_start_ms: u64,
    /// Pause snapshot; consumed on manual /play to resume same song. #88.
    paused_at: Option<(i64, u64)>,
    /// #229: the failed opens in a row and the pending retry
    /// (`failure_retry.rs`); a `Started` ends the run.
    failures: failure_retry::FailureState,
    /// #229: the song a SelectAndPlay or a PlayVideo sent; its `Started`
    /// records it as played (`song_started`). Every Play clears it first.
    record_on_start: Option<i64>,
    /// #229 follow-up: the Plays sent and not answered yet (every Play counts
    /// one, `begin_play`); only the answer to the last one acts.
    pending_plays: failure_backoff::PlayAnswers,
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
    /// The loaded NDI SDK (Windows only): `SP-program`'s sender and the NDI
    /// input "OBS manuál" use it. #221 lane 3: a playlist pipeline has no NDI
    /// sender of its own.
    #[cfg(windows)]
    ndi_backend: Option<SharedNdiBackend>,
    /// For sending text source updates to OBS.
    obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
    /// cg OBS's events — #213: the remote-control facade re-emits the scene ones.
    obs_event_tx: broadcast::Sender<ObsEvent>,
    /// For sending title show/hide commands to Resolume hosts.
    resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
    /// WebSocket broadcast — forwards `NowPlaying` and `PlaybackStateChanged`
    /// messages to the dashboard.
    ws_event_tx: broadcast::Sender<ServerMsg>,
    /// Presenter stage-display client; None = push disabled.
    presenter_client: Option<Arc<crate::presenter::PresenterClient>>,
    /// Reference for mapping `Instant` (pipeline-thread heartbeat) →
    /// `DateTime<Utc>` (dashboard timestamps). Captured at engine
    /// construction.
    instant_origin: (std::time::Instant, chrono::DateTime<chrono::Utc>),
    /// Shared registry holding the latest NDI health snapshot per pipeline.
    /// Cloned into `AppState` so the API layer reads without going through
    /// the engine. Mirrors the `Arc<ResolumeRegistry>` pattern from PR #54.
    ndi_health_registry: std::sync::Arc<crate::playback::ndi_health::NdiHealthRegistry>,
    /// Shared dantesync clock health (#146). Written by the clock-health
    /// poller (spawned in `lib.rs::start`); read when building each NDI health
    /// snapshot. Defaults to `no dantesync` until a handle is injected.
    clock_health: std::sync::Arc<std::sync::RwLock<crate::playback::clock_health::ClockHealth>>,
    /// Per-pipeline genlock lock-state event windows (#149, Lane 1). One 60 s
    /// ring of cumulative pacing counters per playlist, pushed at each
    /// heartbeat; the snapshot's `lock_state` / `lock_reason` are derived from
    /// the differenced counts. Engine-thread-local, not shared.
    lock_windows: HashMap<i64, crate::playback::lock_state::EventWindow>,
    /// Per-playlist live preview tap registry (#15 part 2). Cloned into
    /// `AppState` so `GET /api/v1/playback/{id}/preview.jpg` reads the same
    /// taps the pipeline decode loops write. Each pipeline gets a `PreviewTap`
    /// from it at spawn. Defaults to an empty registry until
    /// `set_preview_registry` shares the one `lib.rs::start` owns.
    preview_registry: std::sync::Arc<crate::playback::preview::PreviewRegistry>,
    /// #215: the program bus (set by `start_program`), asked whether a playlist
    /// that left program must keep playing through a transition.
    program: std::sync::OnceLock<Arc<crate::playback::program_bus::ProgramBus>>,
    /// #221 L4b: the playlists on air as the playback authority last diffed
    /// them, read by its stale check (`program_authority.rs`).
    on_air: program_authority::OnAirPlaylists,
}

/// Construction-time configuration for [`PlaybackEngine`]. Bundling these
/// fields into a struct avoids the 8-arg positional `new` that prompted
/// `#[allow(clippy::too_many_arguments)]` and makes call sites readable
/// at every test setup.
pub struct PlaybackEngineConfig {
    pub pool: SqlitePool,
    pub cache_dir: PathBuf,
    pub obs_event_tx: broadcast::Sender<ObsEvent>,
    pub obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
    pub resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
    pub ws_event_tx: broadcast::Sender<ServerMsg>,
    pub presenter_client: Option<Arc<crate::presenter::PresenterClient>>,
    pub ndi_health_registry: std::sync::Arc<crate::playback::ndi_health::NdiHealthRegistry>,
}

impl PlaybackEngine {
    /// Create a new playback engine. Loads the NDI SDK once on Windows (for
    /// `SP-program` and the NDI input).
    pub fn new(cfg: PlaybackEngineConfig) -> Self {
        let PlaybackEngineConfig {
            pool,
            cache_dir,
            obs_event_tx,
            obs_cmd_tx,
            resolume_tx,
            ws_event_tx,
            presenter_client,
            ndi_health_registry,
        } = cfg;
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
                    warn!(%e, "NDI SDK not available — no SP-program, no NDI input");
                    None
                }
            }
        };

        let instant_origin = (std::time::Instant::now(), Utc::now());

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
            presenter_client,
            instant_origin,
            ndi_health_registry,
            clock_health: std::sync::Arc::new(std::sync::RwLock::new(
                crate::playback::clock_health::ClockHealth::default(),
            )),
            lock_windows: HashMap::new(),
            preview_registry: std::sync::Arc::new(crate::playback::preview::PreviewRegistry::new()),
            program: std::sync::OnceLock::new(),
            on_air: Default::default(),
        }
    }

    /// Inject the shared live-preview registry (#15 part 2) that
    /// `lib.rs::start` also hands to `AppState`, so the HTTP
    /// `GET /api/v1/playback/{id}/preview.jpg` handler and the pipeline decode
    /// loops share one registry. Must be called before pipelines are spawned
    /// (new pipelines register a tap into it at spawn).
    pub fn set_preview_registry(
        &mut self,
        registry: std::sync::Arc<crate::playback::preview::PreviewRegistry>,
    ) {
        self.preview_registry = registry;
    }

    /// Inject the shared dantesync clock-health handle written by the poller
    /// spawned in `lib.rs::start` (#146). Until this is called, snapshots
    /// carry the default `no dantesync` health.
    pub fn set_clock_health(
        &mut self,
        handle: std::sync::Arc<std::sync::RwLock<crate::playback::clock_health::ClockHealth>>,
    ) {
        self.clock_health = handle;
    }

    /// Receive the next pipeline event (for use in external select! loops).
    pub async fn recv_pipeline_event(&mut self) -> Option<(i64, PipelineEvent)> {
        self.event_rx.recv().await
    }

    /// Re-sync the wall title when a scene becomes program for a Playing
    /// pipeline. The 1.5 s title-show task aborted with "title suppressed —
    /// off program" if the scene was off then, so without this the wall shows
    /// a stale title. It goes through the same `Resync` as a recovery, with
    /// the song's title clock: a scene-on outside the window (a song that has
    /// not started yet, its first 1.5 s, its last 3.5 s) shows no title, and a
    /// title that is already up is not faded again. The song's timers are
    /// armed again for what is still ahead, at the decision instant and
    /// before the send (#217 addendum 3).
    async fn push_title_for_playing(&mut self, playlist_id: i64, video_id: i64) {
        // Re-arm at the decision's own instant, before the send: no await
        // between them, so the timers and the Resync never disagree about
        // which side of a show / hide instant the wall is on (review round 3).
        let decided = self.decide_wall_title().await;
        let now = decided
            .as_ref()
            .map_or_else(tokio::time::Instant::now, |(_, at)| *at);
        self.rearm_title_timers(playlist_id, video_id, now);
        if let Some((title, _)) = decided {
            title::send_resync(self.obs_cmd_tx.as_ref(), &self.resolume_tx, title.clone()).await;
            info!(
                playlist_id,
                video_id,
                ?title,
                "title re-synced on scene-go-on"
            );
        }
    }

    /// Put a playlist on or off program (#221 L4b: the playback authority's
    /// `OnProgram`, `program_authority.rs`). On program, fires
    /// `VideosAvailable` then `SceneOn` (folded so every caller goes through
    /// the same sequence). Off program, fires `SceneOff`, or holds it through
    /// its transition (`scene_off.rs`).
    pub async fn handle_scene_change(&mut self, playlist_id: i64, on_program: bool) {
        let before = self.scene_snapshot(playlist_id); // for `broadcast_scene_flip`
        // Going off-program cancels title timers and clears Resolume
        // state (prevents last-write-wins bleed between playlists on
        // the shared `#sp-title` / `#sp-subs` clips — 2026-04-19 event).
        let went_off_program = if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            // Release pairs with Acquire in the title-show task.
            let prev = pp.scene_active.swap(on_program, Ordering::Release);
            if on_program {
                pp.end_hold(); // #215: back on program, no pause is pending
            }
            prev && !on_program
        } else {
            false
        };
        if went_off_program {
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.cancel_title_timers();
                // Design record 5863318980 item 2: the lyrics stay loaded
                // through the #215 hold (a scene back on program resumes its
                // lines; the pause drops them). The wall is cleared below and
                // the stage display moves on, so their dedup keys go too.
                pp.last_resolume_subtitles_signature = None;
                pp.last_presenter_text = None;
            }
            self.wall_after_scene_off().await; // #221 L4b: or re-synced to one on program
        }

        if on_program {
            self.apply_event(playlist_id, PlayEvent::VideosAvailable)
                .await;
            self.apply_event(playlist_id, PlayEvent::SceneOn).await;
            self.retry_came_on_program(playlist_id); // #229: an ON that sent no Play

            // #45 — re-push title for an already-Playing pipeline that
            // just gained program. The 1.5 s post-Started title-show task
            // suppressed itself if scene_active was false at that boundary;
            // there is no other path that re-pushes it.
            let video_id = self
                .pipelines
                .get(&playlist_id)
                .and_then(|pp| match pp.state {
                    PlayState::Playing { video_id } => Some(video_id),
                    _ => None,
                });
            if let Some(video_id) = video_id {
                self.push_title_for_playing(playlist_id, video_id).await;
            }
            // #221 (review rounds 1-2): the wall owner's ON re-syncs the
            // whole wall (`scene_off::wall_after_owner_on`).
            if self.on_air.owner() == Some(playlist_id) {
                self.wall_after_owner_on(playlist_id, video_id.is_some())
                    .await;
            }
        } else {
            self.scene_off(playlist_id).await; // #215: held through a transition
        }
        self.broadcast_scene_flip(playlist_id, before); // #221 L4b: the WS state
    }

    /// Re-wake pipelines parked in `WaitingForScene` after the download
    /// worker finishes normalizing a video. Fixes the stuck-WaitingForScene
    /// bug (0.11.0): on first boot after the V4 migration reset
    /// `normalized = 0`, the scene-on event fires `SelectAndPlay` on an
    /// empty DB, pipeline parks in `WaitingForScene` with no listener for
    /// download completion. This method is that missing listener — driven
    /// by the `processed:{id}` broadcast from `DownloadWorker`.
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
                    && pp.scene_active.load(Ordering::Acquire)
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

    /// Handle a user command (skip, mode change, etc.).
    pub async fn handle_command(&mut self, playlist_id: i64, cmd: PlayEvent) {
        // #225 unit 2: a mode the playlist's row now holds (`playlist_mode.rs`);
        // the state machine ignores it, so that is all a SetMode does.
        if let PlayEvent::SetMode(mode) = &cmd {
            self.apply_mode(playlist_id, *mode);
            return;
        }
        // #215: a skip of a playlist held off program starts no song there.
        if matches!(cmd, PlayEvent::Skip) && self.pause_if_held(playlist_id, "skipped").await {
            return;
        }
        // #229: a skip while a failed open's retry waits tries the next song now.
        if matches!(cmd, PlayEvent::Skip) && self.skip_backoff(playlist_id).await {
            return;
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
                    pp.end_hold(); // #215: the operator's pick plays, off program too
                    info!(
                        playlist_id,
                        video_id, %video_path, %audio_path,
                        "Previous → replaying song from history"
                    );
                    pp.begin_play(0);
                    pp.pipeline.send(PipelineCommand::Play {
                        video: video_path.into(),
                        audio: audio_path.into(),
                        start_position_ms: None,
                    });

                    // Broadcast the state change so the dashboard updates: an
                    // off-program Previous shows WaitingForScene (#170), with
                    // the raw transport (#201).
                    self.broadcast_state(playlist_id);
                    self.publish_open_failures(playlist_id); // #229: the Play ended a retry
                }
                self.resync_after_play(playlist_id).await;
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

        // #229: off program, or paused: a failed open's retry waits no more.
        if matches!(event, PlayEvent::SceneOff) {
            pp.failures.cancel_retry();
        }
        let mode = pp.mode;
        let old_state = pp.state.clone();
        let (new_state, action) = old_state.clone().transition(event, mode);
        pp.state = new_state.clone();

        if let Some(action) = action {
            self.execute_action(playlist_id, action).await;
        }

        // After the action (which may itself mutate the state to Playing),
        // broadcast the final state, in its scene-aware wire state
        // (`broadcast_state`, #170), if it differs from the pre-transition
        // state. The pipeline always exists here (`execute_action` never
        // removes one; the no-pipeline case returned at the top).
        if self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| pp.state != old_state)
        {
            self.broadcast_state(playlist_id);
        }
        self.publish_open_failures(playlist_id); // #229: the run as it is now
    }

    /// Cache the video's song/artist/duration and broadcast `NowPlaying`
    /// with `position_ms: 0`. Called when a pipeline reports a `Started`
    /// event (i.e. playback just began).
    #[cfg_attr(test, mutants::skip)] // DB/WS glue
    async fn broadcast_now_playing_on_start(&mut self, playlist_id: i64, duration_ms: u64) {
        let video_id = match self
            .pipelines
            .get(&playlist_id)
            .and_then(|pp| pp.current_video_id)
        {
            Some(id) => id,
            None => return,
        };

        crate::now_playing::global().set(playlist_id, video_id); // #177 bind panel

        let (song, artist) = match title::get_video_title_info(&self.pool, video_id).await {
            Ok(Some(pair)) => pair,
            _ => (String::new(), String::new()),
        };
        let suppress_en = crate::db::models::get_video_suppress_resolume_en(&self.pool, video_id)
            .await
            .unwrap_or(false);
        let lyrics_reference = crate::db::models::get_video_lyrics_reference(&self.pool, video_id)
            .await
            .unwrap_or(false);

        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.cached_song = song.clone();
            pp.cached_artist = artist.clone();
            pp.cached_duration_ms = duration_ms;
            pp.cached_suppress_en = suppress_en;
            pp.cached_lyrics_reference = lyrics_reference;
            pp.last_now_playing_broadcast = Some(Instant::now());
        }

        self.send_dashboard(ServerMsg::NowPlaying {
            playlist_id,
            video_id,
            song,
            artist,
            position_ms: 0,
            duration_ms,
        });
    }

    // `clear_lyrics_display` lives in `clear_lyrics.rs`.
    // `dispatch_lyrics_if_changed` and `maybe_broadcast_position_update`
    // live in `position_update.rs` (extracted to keep this file lean).

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
                // #229: leave out the song just sent and the run's failed ones.
                let avoid = self
                    .pipelines
                    .get(&playlist_id)
                    .map(|pp| pp.failures.run.avoid(current))
                    .unwrap_or_default();

                let pick =
                    VideoSelector::select_next(&self.pool, playlist_id, mode, current, &avoid);
                match pick.await {
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
                                    // #217 addendum 3: even the same video gets
                                    // a new title clock, at its new Started.
                                    pp.begin_play(0);
                                    pp.pipeline.send(PipelineCommand::Play {
                                        video: video_path.into(),
                                        audio: audio_path.into(),
                                        start_position_ms: None,
                                    });
                                    // #229: played once it starts (`song_started`).
                                    pp.record_on_start = Some(video_id);
                                }
                                self.resync_after_play(playlist_id).await;
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
                let mut played = false;
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    if let Some(video_id) = pp.current_video_id {
                        debug!(playlist_id, "replaying current video");
                        match crate::db::models::get_song_paths(&self.pool, video_id).await {
                            Ok(Some((video_path, audio_path))) => {
                                pp.begin_play(0);
                                pp.pipeline.send(PipelineCommand::Play {
                                    video: video_path.into(),
                                    audio: audio_path.into(),
                                    start_position_ms: None,
                                });
                                played = true;
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
                if played {
                    self.resync_after_play(playlist_id).await;
                }
            }

            PlayAction::Pause => {
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.paused_at = pp.current_video_id.map(|v| (v, pp.cached_position_ms));
                    pp.pipeline.send(PipelineCommand::Pause);
                    // A paused song writes nothing more (design record
                    // 5863318980 items 1c + 2): its title timers and a
                    // hold's re-check go, and its lyrics, which a scene-off
                    // kept through the hold. A resume's `Started` reloads them.
                    pp.cancel_title_timers();
                    pp.end_hold();
                    pp.lyrics_state = None;
                    debug!(playlist_id, paused_at = ?pp.paused_at, "paused pipeline");
                }
                // On program, a paused song's title and line are not due: the
                // wall says so now, as any later re-sync would (review rounds
                // 1-2). Off program (the hold's end) nothing of it is up.
                let on_program = self
                    .pipelines
                    .get(&playlist_id)
                    .is_some_and(|pp| pp.scene_active.load(Ordering::Acquire));
                if on_program {
                    self.clear_lyrics_display(playlist_id);
                }
                self.resync_after_play(playlist_id).await;
            }

            PlayAction::SendBlack => {
                if let Some(pp) = self.pipelines.get(&playlist_id) {
                    pp.pipeline.send(PipelineCommand::Stop);
                    debug!(playlist_id, "sent black / stopped pipeline");
                }
            }

            PlayAction::Stop => {
                crate::now_playing::global().clear(playlist_id); // #177
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
#[path = "dispatch_lyrics_tests.rs"]
mod dispatch_lyrics_tests;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
#[cfg(test)]
#[path = "tests_engine_setters.rs"]
mod tests_engine_setters;
#[cfg(test)]
#[path = "tests_history.rs"]
mod tests_history;
#[cfg(test)]
#[path = "tests_hold.rs"]
mod tests_hold;
#[cfg(test)]
#[path = "tests_play_video.rs"]
mod tests_play_video;
#[cfg(test)]
#[path = "tests_runtime_pipeline.rs"]
mod tests_runtime_pipeline;
#[cfg(test)]
#[path = "tests_scene_change.rs"]
mod tests_scene_change;
#[cfg(test)]
#[path = "tests_song_end.rs"]
mod tests_song_end;
#[cfg(test)]
#[path = "tests_ws_replay.rs"]
mod tests_ws_replay;
