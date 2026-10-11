//! SongPlayer server — all business logic.

pub mod ai;
mod ai_proxy_watchdog;
pub mod api;
pub mod background_hold; // #230: a sp-90s press holds the background jobs
pub mod dabing;
pub mod db;
pub mod diag; // #223 S0: /api/v1/diag/* measurement benches
pub mod downloader;
mod embedded_scripts; // #207: the Python tool scripts a worker writes into tools_dir
mod engine_command;
mod engine_dispatch;
pub use engine_command::EngineCommand;
pub mod gemini_api; // #136: the Gemini key-list contract every Gemini caller shares
pub mod lyrics;
pub mod mdns;
pub mod metadata;
pub mod now_playing;
pub mod obs;
mod obs_bridge;
pub mod paid_ai; // #229 item C: the ONE switch every paid AI call site asks
pub mod panic_hook;
pub mod peer; // #229: the node exchange (serve what this node has, ask peers first)
pub mod playback;
pub mod playlist;
pub mod presenter;
pub mod process_start; // #196: process-start instant for /api/v1/status.uptime_s
pub mod remote; // #213: the Companion remote control (obs-websocket 5 subset → program bus)
pub mod reprocess;
pub mod resolume;
pub mod shutdown;
mod song_input; // #136: a stem / dub job's input, re-read after the heavy slot
mod song_relink; // #136: stems / dub left under an old name → the audio's name
pub mod startup;
pub mod stems;
pub mod test_item; // #228: camera-box's measurement clip as a one-item test playlist
#[cfg(test)]
mod test_log; // a scoped log capture shared by the tests
mod tools_ready; // #144: publish the ready tools, then the slow follow-ups
pub mod video_upgrade; // #223 S11: a cached song's video upgraded in place

pub use panic_hook::install_panic_hook;

use std::path::PathBuf;
use std::sync::Arc;

use sp_core::ws::ServerMsg;
use sqlx::{Row, SqlitePool};
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{info, warn};

use crate::downloader::tools::ToolPaths;

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

/// A request to sync a playlist with its YouTube source.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub playlist_id: i64,
    pub youtube_url: String,
}

/// Shared state passed to all Axum handlers and background workers.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub event_tx: broadcast::Sender<ServerMsg>,
    pub engine_tx: mpsc::Sender<EngineCommand>,
    pub obs_state: Arc<RwLock<obs::ObsState>>,
    pub tools_status: Arc<RwLock<ToolsStatus>>,
    pub tool_paths: Arc<RwLock<Option<ToolPaths>>>,
    pub sync_tx: mpsc::Sender<SyncRequest>,
    pub resolume_tx: mpsc::Sender<resolume::ResolumeCommand>,
    /// Directory where cached media and lyrics JSON files are stored.
    pub cache_dir: PathBuf,
    pub ai_proxy: Arc<ai::proxy::ProxyManager>,
    pub ai_client: Arc<ai::client::AiClient>,
    /// Presenter HTTP client; None = push disabled. See `presenter` module.
    pub presenter_client: Option<Arc<presenter::PresenterClient>>,
    /// Resolume registry exposing per-host health snapshots.
    pub resolume_registry: Arc<resolume::ResolumeRegistry>,
    /// Per-pipeline health snapshots (`/api/v1/ndi/health`).
    pub ndi_health_registry: Arc<playback::ndi_health::NdiHealthRegistry>,
    /// Per-playlist live preview tap registry (#15 part 2).
    /// `GET /api/v1/playback/{id}/preview.jpg` reads it; the playback engine +
    /// pipeline decode loops share the same registry (an idle tap costs one
    /// atomic load per decoded frame, and never touches the NDI submit path).
    pub preview_registry: Arc<playback::preview::PreviewRegistry>,
    /// #209: the process-wide program bus (NDI `SP-program`, SongPlayer = master switcher).
    pub program_bus: Arc<playback::program_bus::ProgramBus>,
    /// LAN `sp.local` advertisement status (#51) — written by the mDNS task,
    /// read by `/api/v1/status` so the dashboard shows the offline-LAN URL.
    pub lan_status: mdns::LanStatusHandle,
    /// #136: the ONE metadata provider chain — the same `Arc` the download and
    /// reprocess workers use; `status.metadata` + the probe route read it.
    pub metadata_chain: Arc<metadata::ProviderChain>,
    /// #223 S0: `POST /api/v1/diag/decode-bench`'s sample dir and one-run gate.
    pub decode_bench: Arc<diag::decode_bench::DecodeBench>,
}

/// Status of external tool availability.
#[derive(Debug, Clone, Default)]
pub struct ToolsStatus {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
    /// yt-dlp has a working JS runtime (Deno) for YouTube's n-challenge (#189).
    pub js_runtime_ok: bool,
    /// Bundled Deno version, when present.
    pub deno_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

/// Configuration for the server startup.
pub struct ServerConfig {
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    pub port: u16,
    /// Directory containing the WASM frontend (`dist/`). If set, serves static files.
    pub dist_dir: Option<PathBuf>,
}

impl ServerConfig {
    /// The data dir: the DB's own dir (`C:\ProgramData\SongPlayer` on the
    /// box). The crash log, `cookies.txt` and the decode bench's samples
    /// (`bench\`) live there, outside the media cache. A bare DB file name
    /// gives the current dir.
    pub fn data_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("songplayer.db"),
            cache_dir: PathBuf::from("cache"),
            port: sp_core::config::DEFAULT_API_PORT,
            dist_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the server. Blocks until shutdown signal.
///
/// Orchestrates all subsystems:
/// 1. SQLite pool + migrations
/// 2. Broadcast channels for events
/// 3. Shared state (incl. tool_paths + sync channel)
/// 4. The metadata provider chain (download + reprocess workers + API)
/// 5. Tools manager (yt-dlp + FFmpeg) + download worker
/// 6. Sync handler (playlist sync worker)
/// 7. OBS WebSocket client
/// 8. Reprocess worker (on the same metadata chain)
/// 9. Resolume workers
/// 10. Playback engine
/// 11. Axum HTTP server
/// 12. Shutdown signal
pub async fn start(
    config: ServerConfig,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<(), anyhow::Error> {
    // #196: record the process start for `/api/v1/status.uptime_s`. Idempotent.
    crate::process_start::mark_started();

    // #203: raise SongPlayer to HIGH_PRIORITY_CLASS so the NDI SDK compression
    // threads pre-empt the contained heavy children (stems / lyrics / dub).
    crate::process_start::set_high_priority_class();

    // Install the panic hook FIRST so any panic during startup or steady-state
    // is captured to a durable crash file before release `panic = "abort"`
    // kills the process (#156). Idempotent: the Tauri shell installs it earlier
    // when present, and the internal `Once` makes the double call safe.
    crate::install_panic_hook(config.data_dir().join("songplayer-panic.log"));

    // 1. Database
    let pool = db::startup_open::open(&config.db_path).await?; // #229: waits out a lock
    db::run_migrations(&pool).await?;
    startup::ensure_live_playlist_exists(&pool).await?;
    startup::ensure_dabing_playlist_exists(&pool).await?; // #180 dubbing D1
    info!("database ready");
    crate::process_start::apply_min_working_set(&pool).await; // #147 r9: hard min working set

    // Self-heal cache: delete legacy single-mp4s, delete orphans,
    // re-link complete pairs. Non-fatal on error.
    if let Err(e) = startup::self_heal_cache(&pool, &config.cache_dir).await {
        tracing::warn!("self-heal cache failed (non-fatal): {e}");
    }

    // Self-heal stored metadata: re-run the emoji sanitizer over stored data. Non-fatal.
    if let Err(e) = startup::self_heal_emoji_metadata(&pool).await {
        tracing::warn!("self-heal emoji metadata failed (non-fatal): {e}");
    }

    // Self-heal stored metadata: repair rows whose `song` was written empty by a
    // since-fixed metadata bug — re-derive from the title. Non-fatal.
    if let Err(e) = startup::self_heal_empty_song_metadata(&pool).await {
        tracing::warn!("self-heal empty-song metadata failed (non-fatal): {e}");
    }

    // Round G0 (#184): re-open unsupported stem rows within the raised 120-min cap. Non-fatal.
    if let Err(e) = startup::requeue_unsupported_stems(&pool).await {
        tracing::warn!("stems re-queue within cap failed (non-fatal): {e}");
    }

    // 2. Channels
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let (event_tx, _) = broadcast::channel::<ServerMsg>(256);
    let (engine_tx, mut engine_rx) = mpsc::channel::<EngineCommand>(64);

    // 3. Shared state
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let tools_status = Arc::new(RwLock::new(ToolsStatus::default()));
    let tool_paths: Arc<RwLock<Option<ToolPaths>>> = Arc::new(RwLock::new(None));
    let (sync_tx, mut sync_rx) = mpsc::channel::<SyncRequest>(64);
    let (resolume_cmd_tx, mut resolume_cmd_rx) = mpsc::channel::<resolume::ResolumeCommand>(64);

    // Read AI settings from DB or use defaults
    let ai_api_url = db::models::get_setting(&pool, sp_core::config::SETTING_AI_API_URL)
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_AI_API_URL.to_string());
    let ai_model = db::models::get_setting(&pool, sp_core::config::SETTING_AI_MODEL)
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_AI_MODEL.to_string());

    let ai_settings = ai::AiSettings {
        api_url: ai_api_url,
        api_key: None,
        model: ai_model,
        system_prompt_extra: None,
    };
    let ai_client = Arc::new(ai::client::AiClient::new(ai_settings));
    let presenter_client = presenter::build_from_settings(&pool).await?;

    // 3b. NDI health registry — constructed before AppState so both the engine
    // (writer) and the AppState (reader) can hold an Arc to the same instance.
    let ndi_health_registry = Arc::new(playback::ndi_health::NdiHealthRegistry::new());
    // #15 part 2: one live-preview tap registry shared by AppState (route) +
    // the engine (registers a tap per pipeline at spawn; the decode loops feed
    // it). Idle taps cost one atomic load per decoded frame.
    let preview_registry = Arc::new(playback::preview::PreviewRegistry::new());
    let program_bus = Arc::new(playback::program_bus::ProgramBus::new()); // #209

    // #51: shared LAN sp.local status — the mDNS task (spawned after AppState)
    // writes it, `/api/v1/status` reads it. Starts empty until the task
    // detects the LAN IP and registers the record.
    let lan_status = mdns::new_status_handle();

    // 3b'. dantesync clock health (#146). Shared handle: the 1 Hz poller writes
    // it, the playback engine reads it into every NDI health snapshot. Never
    // blocks playback — a missing endpoint just reads `no dantesync`.
    let clock_health = Arc::new(std::sync::RwLock::new(
        playback::clock_health::ClockHealth::default(),
    ));
    {
        let url = std::env::var("DANTESYNC_STATUS_URL")
            .unwrap_or_else(|_| playback::clock_health::DANTESYNC_STATUS_URL_DEFAULT.to_string());
        playback::clock_health::spawn_clock_health_poller(
            reqwest::Client::new(),
            url,
            clock_health.clone(),
            shutdown_tx.subscribe(),
        );
    }

    // 3c. Resolume registry — must be created before AppState so the Arc can
    // be stored in state and shared with the health endpoint.
    let resolume_rows =
        sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
            .fetch_all(&pool)
            .await?;
    let resolume_hosts: Vec<(i64, String, u16)> = resolume_rows
        .iter()
        .map(|row| {
            let port: i32 = row.get("port");
            (row.get("id"), row.get("host"), port as u16)
        })
        .collect();
    // Its RecoveryEvent → engine forwarder (the title + line re-sync after a
    // host comes back) is subscribed before the first host driver starts.
    let resolume_registry = Arc::new(playback::recovery::registry_with_forwarder(
        resolume_hosts,
        engine_tx.clone(),
        &shutdown_tx,
    ));

    // 4. The ONE metadata provider chain (#136): Claude, then Gemini on the
    // `gemini_api_key` key list. The download worker, the reprocess worker and
    // the API share this `Arc` — no second, divergent provider list.
    let gemini_key = db::models::get_setting(&pool, "gemini_api_key")
        .await?
        .unwrap_or_default();
    let gemini_model = db::models::get_setting(&pool, "gemini_model")
        .await?
        .unwrap_or_else(|| sp_core::config::DEFAULT_GEMINI_MODEL.to_string());

    let metadata_chain =
        metadata::provider_chain(&pool, ai_client.clone(), &gemini_key, &gemini_model);

    let state = AppState {
        pool: pool.clone(),
        event_tx: event_tx.clone(),
        engine_tx: engine_tx.clone(),
        obs_state: obs_state.clone(),
        tools_status: tools_status.clone(),
        tool_paths: tool_paths.clone(),
        sync_tx: sync_tx.clone(),
        resolume_tx: resolume_cmd_tx.clone(),
        cache_dir: config.cache_dir.clone(),
        ai_proxy: Arc::new(ai::proxy::ProxyManager::new(
            config.cache_dir.clone(),
            ai::proxy::ProxyManager::default_port(),
        )),
        ai_client: ai_client.clone(),
        presenter_client: presenter_client.clone(),
        resolume_registry: resolume_registry.clone(),
        ndi_health_registry: ndi_health_registry.clone(),
        preview_registry: preview_registry.clone(),
        program_bus: program_bus.clone(),
        lan_status: lan_status.clone(),
        metadata_chain: metadata_chain.clone(),
        decode_bench: Arc::new(diag::decode_bench::DecodeBench::new(
            config.data_dir().join("bench"),
        )),
    };
    // #229: this node in the exchange; its routes merge into the router below,
    // its hasher fills the catalog's sha256 cache while the node serves.
    let exchange = peer::Exchange::new(pool.clone(), config.cache_dir.clone());
    tokio::spawn(peer::hasher::run(exchange.clone(), shutdown_tx.subscribe()));
    // #223 S12a: the in-place video upgrade, behind `video_upgrade_enabled`.
    let upgrade_dir = config.cache_dir.clone();
    let upgrade = video_upgrade::worker::run(
        pool.clone(),
        upgrade_dir,
        tool_paths.clone(),
        shutdown_tx.subscribe(),
    );
    tokio::spawn(upgrade);

    // #51: advertise `sp.local` over mDNS so the dashboard stays reachable on
    // the LAN with no internet. Reads `lan_mdns_enabled` (default on); a
    // failure only degrades to no advertisement, it never blocks startup.
    mdns::spawn_lan_mdns(
        pool.clone(),
        config.port,
        lan_status.clone(),
        shutdown_tx.subscribe(),
    )
    .await;

    // Auto-start the CLIProxyAPI child process + start a watchdog that
    // periodically re-launches it if it dies. Without this, every
    // SongPlayer restart (including CI deploys) leaves the proxy
    // unstarted — the description provider, text-merge, and all other
    // Claude calls silently fall through to "no text sources available"
    // for most songs (2026-04-19 event: 100% of in-flight songs failed
    // gather_sources until POST /api/v1/ai/proxy/start was hit manually).
    if state.ai_proxy.is_claude_authenticated() {
        match state.ai_proxy.start().await {
            Ok(()) => info!("ai_proxy: auto-started CLIProxyAPI at boot"),
            Err(e) => warn!("ai_proxy: auto-start failed (watchdog will retry): {e}"),
        }
        let watchdog_proxy = state.ai_proxy.clone();
        let watchdog_shutdown = shutdown_tx.subscribe();
        tokio::spawn(ai_proxy_watchdog::run(watchdog_proxy, watchdog_shutdown));
    } else {
        info!("ai_proxy: not authenticated, skipping auto-start + watchdog");
    }

    // 5. Tools manager
    let tools_dir = config.cache_dir.join("tools");
    let tools_mgr = downloader::tools::ToolsManager::new(tools_dir.clone());

    // Download worker broadcast channel. Hoisted out of the tools-setup
    // task so the engine can subscribe before tools become ready — that
    // way the engine never misses a `processed:<id>` event, which is
    // how a freshly-normalized video rewakes pipelines parked in
    // WaitingForScene after the 0.11 FLAC migration reset the cache.
    let (dl_event_tx, _dl_event_rx_placeholder) = broadcast::channel::<String>(64);
    let dl_event_tx_for_worker = dl_event_tx.clone();

    let tools_sinks = tools_ready::ToolsSinks {
        status: tools_status.clone(),
        paths: tool_paths.clone(),
        events: event_tx.clone(),
    };
    let lyrics_event_tx = event_tx.clone();
    let dl_pool = pool.clone();
    let dl_cache_dir = config.cache_dir.clone();
    // Same directory as the SQLite DB — where a production operator drops
    // cookies.txt (Netscape format) to authenticate yt-dlp downloads (#141).
    let dl_data_dir = config.data_dir();
    let dl_shutdown_tx = shutdown_tx.clone();
    let dl_metadata_chain = metadata_chain.clone();
    let dl_exchange = exchange.clone(); // #229: each download asks the peers first
    let startup_sync_pool = pool.clone();
    let startup_sync_tx = sync_tx.clone();
    let periodic_sync_pool = pool.clone();
    let periodic_sync_tx = sync_tx.clone();
    let periodic_sync_shutdown = shutdown_tx.clone();
    let ytdlp_update_shutdown = shutdown_tx.clone();
    let lyrics_pool = pool.clone();
    let lyrics_cache_dir = config.cache_dir.clone();
    let lyrics_shutdown = shutdown_tx.clone();
    let lyrics_tools_dir = tools_dir;
    let ai_client_for_dl = ai_client.clone();
    let lyrics_ndi_health = ndi_health_registry.clone();
    let lyrics_obs_state = obs_state.clone();
    let lyrics_exchange = exchange.clone(); // #229: each song asks the peers first
    // #14 karaoke stem worker shares the same tools dir + idle-gate handles.
    let stem_pool = pool.clone();
    let stem_tools_dir = lyrics_tools_dir.clone();
    let stem_ndi_health = ndi_health_registry.clone();
    let stem_obs_state = obs_state.clone();
    let stem_shutdown = shutdown_tx.clone();
    let stem_exchange = exchange.clone(); // #229: each separation asks the peers first
    // #183 D4 dub worker: same tools dir + idle-gate handles as the stem worker.
    let dub_pool = pool.clone();
    let dub_tools_dir = lyrics_tools_dir.clone();
    let dub_ndi_health = ndi_health_registry.clone();
    let dub_obs_state = obs_state.clone();
    let dub_shutdown = shutdown_tx.clone();
    tokio::spawn(crate::lyrics::host_commit::run_host_commit_logger(
        shutdown_tx.subscribe(),
    )); // #207: per-minute host commit/pagefile logger (Windows only)
    tokio::spawn(async move {
        match tools_mgr.ensure_tools().await {
            Ok(paths) => {
                let version = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();

                // #189: ship + wire the Deno JS runtime for YouTube's n-challenge
                // and run the startup self-check (memoizes the runtime args for
                // every yt-dlp spawn; logs OK/MISSING loudly).
                let (js_runtime_ok, deno_version) =
                    downloader::ytdlp_cmd::init_js_runtime(&tools_mgr, &paths, &dl_data_dir).await;
                let found = tools_ready::ToolsFound {
                    ytdlp_version: version,
                    js_runtime_ok,
                    deno_version,
                };

                // Everything below runs AFTER the publish, with no lock
                // held: `GET /api/v1/status` answers through it (#144).
                let published = paths.clone();
                let follow_ups = async move {
                    // yt-dlp self-update (#140): the download worker never
                    // updates its own yt-dlp binary, so a box that has been up
                    // for a while silently falls behind YouTube's format
                    // changes (observed: `audio download failed … Requested
                    // format is not available` on a stale 2026.03 build — see
                    // `.claude/rules/youtube-cookies.md`). One-shot update
                    // right after tools are ready, then a shutdown-aware
                    // periodic re-update — never fatal, a stale yt-dlp should
                    // degrade, not crash the server.
                    let version_before = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();
                    match tools_mgr.update_ytdlp().await {
                        Ok(()) => {
                            let version_after = tools_mgr.ytdlp_version(&paths.ytdlp).await.ok();
                            info!(
                                version_before = ?version_before,
                                version_after = ?version_after,
                                "yt-dlp self-update: startup check complete"
                            );
                        }
                        Err(e) => warn!("yt-dlp self-update: startup check failed: {e}"),
                    }
                    // Shared with the download worker below and the video
                    // upgrade: an update never runs while one runs yt-dlp.
                    let ytdlp_lock: downloader::YtdlpLock = downloader::ytdlp_lock();
                    let ytdlp_interval_secs = ytdlp_update_interval_secs();
                    tokio::spawn(periodic_ytdlp_update(
                        dl_pool.clone(),
                        tools_mgr,
                        paths.ytdlp.clone(),
                        ytdlp_interval_secs,
                        ytdlp_lock.clone(),
                        ytdlp_update_shutdown.subscribe(),
                    ));
                    info!(
                        interval_secs = ytdlp_interval_secs,
                        "periodic yt-dlp self-update worker started"
                    );

                    // Defensive self-heal for #40: any normalized=1 row whose
                    // FLAC is not at 48 kHz would explode in
                    // SplitSyncedDecoder. Flip them back to normalized=0 so
                    // the download worker re-normalizes under the post-#38
                    // pipeline (which pins -ar 48000 -ac 2).
                    if let Err(e) = startup::flip_wrong_sample_rate_rows(
                        &startup_sync_pool,
                        startup::probe_sample_rate_symphonia,
                    )
                    .await
                    {
                        tracing::warn!("self-heal: sample-rate sweep failed: {e}");
                    }

                    // Startup sync fires AFTER tools are ready so the sync
                    // worker doesn't silently drop the requests.
                    if let Err(e) =
                        startup::startup_sync_active_playlists(&startup_sync_pool, &startup_sync_tx)
                            .await
                    {
                        tracing::warn!("startup sync enqueue failed: {e}");
                    }

                    // Periodic re-sync (#139): the one-shot startup sync above
                    // only ever fires once, so a video added to a YouTube
                    // playlist later would never be picked up without an
                    // operator manually hitting the sync button. Spawned only
                    // once tools are ready, same as the startup sync itself.
                    let periodic_interval_secs = playlist_sync_interval_secs();
                    tokio::spawn(periodic_playlist_sync(
                        periodic_sync_pool,
                        periodic_sync_tx,
                        periodic_interval_secs,
                        periodic_sync_shutdown.subscribe(),
                    ));
                    info!(
                        interval_secs = periodic_interval_secs,
                        "periodic playlist sync worker started"
                    );

                    let lyrics_ytdlp = paths.ytdlp.clone();
                    let lyrics_python = paths.python.clone();
                    let dl_worker = downloader::DownloadWorker::new(
                        dl_pool,
                        paths,
                        dl_cache_dir,
                        dl_data_dir,
                        dl_metadata_chain,
                        dl_event_tx_for_worker,
                        ytdlp_lock,
                    )
                    .with_peer(dl_exchange);
                    tokio::spawn(dl_worker.run(dl_shutdown_tx.subscribe()));
                    info!("download worker started");

                    // Lyrics worker
                    let lyrics_pool_for_loop = lyrics_pool.clone();
                    let lyrics_worker = lyrics::LyricsWorker::new(
                        lyrics_pool,
                        lyrics_cache_dir,
                        lyrics_ytdlp,
                        lyrics_python,
                        lyrics_tools_dir,
                        Some(ai_client_for_dl),
                        lyrics_event_tx.clone(),
                        lyrics_ndi_health,
                        lyrics_obs_state,
                    )
                    .with_peer(lyrics_exchange);
                    let current_processing_handle = lyrics_worker.current_processing();
                    tokio::spawn(lyrics_worker.run(lyrics_shutdown.subscribe()));
                    info!("lyrics worker started");

                    // Lyrics queue-update broadcast loop (every 2s → WS clients)
                    tokio::spawn(crate::lyrics::worker::queue_update_loop(
                        lyrics_pool_for_loop,
                        lyrics_event_tx.clone(),
                        current_processing_handle,
                        lyrics_shutdown.subscribe(),
                    ));

                    // Karaoke stem worker (#14) — separates the catalog into
                    // vocals + instrumental sidecars under the SAME #154 idle gate,
                    // lowest priority (after lyrics).
                    let stem_worker = crate::stems::StemWorker::new(
                        stem_pool,
                        stem_tools_dir,
                        stem_ndi_health,
                        stem_obs_state,
                    )
                    .with_peer(stem_exchange);
                    tokio::spawn(stem_worker.run(stem_shutdown.subscribe()));
                    // (StemWorker::run logs "stem worker started" once it is live.)

                    // #183 D4: dub-synthesis worker (Gemini Live Translate) — same
                    // tools dir + heavy slot, BELOW_NORMAL, never gating playback.
                    let dub_worker = crate::dabing::DubWorker::new(
                        dub_pool,
                        dub_tools_dir,
                        dub_ndi_health,
                        dub_obs_state,
                    );
                    tokio::spawn(dub_worker.run(dub_shutdown.subscribe()));
                    // (DubWorker::run logs "dub worker started" once it is live.)
                };
                tools_ready::publish_then(tools_sinks, published, found, follow_ups).await;
            }
            Err(e) => {
                tracing::error!("tools setup failed: {e}");
            }
        }
    });

    // 6. Sync handler — receives SyncRequests and calls playlist::sync_playlist
    let sync_pool = pool.clone();
    let sync_tool_paths = tool_paths.clone();
    tokio::spawn(async move {
        while let Some(req) = sync_rx.recv().await {
            // #230: held — dropped; the periodic sync re-enqueues after the hold.
            if background_hold::holds(&sync_pool, background_hold::Job::Sync).await {
                continue;
            }
            let paths = sync_tool_paths.read().await;
            let Some(ref tp) = *paths else {
                warn!(
                    playlist_id = req.playlist_id,
                    "sync request received but tools not yet available, dropping — the periodic sync re-enqueues"
                );
                continue;
            };
            let ytdlp = tp.ytdlp.clone();
            drop(paths); // release read lock before awaiting sync

            match playlist::sync_playlist(&sync_pool, req.playlist_id, &req.youtube_url, &ytdlp)
                .await
            {
                Ok(new_count) => {
                    info!(
                        playlist_id = req.playlist_id,
                        new_count, "playlist sync complete"
                    );
                }
                Err(e) => {
                    warn!(playlist_id = req.playlist_id, "playlist sync failed: {e}");
                }
            }
        }
    });

    // 7. OBS WebSocket client (#221 L6: no scene detection; it serves the
    // facade's forwards and manual press, and the title text).
    let obs_side = obs_bridge::start_obs(&pool, &obs_state, &shutdown_tx).await?;

    // 8. Reprocess worker — on the SAME metadata chain as the download worker
    // (#136: it used to get Gemini alone, so it could never repair a row);
    // #229: a peer's title first.
    let reprocess_worker =
        reprocess::ReprocessWorker::new(pool.clone(), metadata_chain, config.cache_dir.clone())
            .with_peer(exchange.clone());
    tokio::spawn(reprocess_worker.run(shutdown_tx.subscribe()));

    // 9. Resolume command forwarding (registry was built before AppState above).
    // Forward commands from the shared channel to all host workers.
    // Uses try_send to avoid blocking the broadcast loop on a slow Resolume host;
    // dropped messages are logged at debug level for observability.
    let resolume_senders = resolume_registry.host_senders();
    tokio::spawn(async move {
        while let Some(cmd) = resolume_cmd_rx.recv().await {
            for tx in &resolume_senders {
                if let Err(e) = tx.try_send(cmd.clone()) {
                    tracing::debug!(%e, "Resolume command dropped (channel full or closed)");
                }
            }
        }
    });

    // #14 karaoke: seed the process-global live control before pipelines spawn.
    crate::stems::control::init_from_settings(&pool).await;
    // #223 S3b: `video_hw_decode` applied before any pipeline opens a song.
    crate::playback::video_decode::start(&pool, &shutdown_tx).await;

    // 10. Playback engine (bridges API commands to the engine state machine)
    let mut engine = playback::PlaybackEngine::new(playback::PlaybackEngineConfig {
        pool: pool.clone(),
        cache_dir: config.cache_dir.clone(),
        obs_event_tx: obs_side.event_tx,
        obs_cmd_tx: obs_side.cmd_tx,
        resolume_tx: resolume_cmd_tx,
        ws_event_tx: event_tx.clone(),
        presenter_client,
        ndi_health_registry,
    });
    // Inject the shared dantesync clock-health handle into every health snapshot (#146).
    engine.set_clock_health(clock_health);
    // #15 part 2: share the preview registry BEFORE pipelines spawn (each
    // pipeline registers a preview tap into it at spawn).
    engine.set_preview_registry(preview_registry.clone());

    // A pipeline for every active playlist, in playlist.id order and its row's
    // mode (#225 unit 2), before `start_program` and the engine loop. #221 lane
    // 3: a pipeline has no NDI sender of its own (it feeds the program bus),
    // so this waits for nothing; `start_program` then creates SP-program, the
    // only NDI sender, after its #196 port wait.
    let active_playlists = db::models::get_active_playlists(&pool)
        .await
        .unwrap_or_default();
    // #242: every playlist's own sound, before a pipeline opens a song.
    playback::playlist_fx::load_all(&pool, playback::playlist_fx::global()).await;
    engine.create_startup_pipelines(&active_playlists);
    engine.start_program(program_bus, &shutdown_tx).await;

    // Engine subscribes to the download worker's broadcast so that
    // `processed:<youtube_id>` events can rewake pipelines stuck in
    // `WaitingForScene`. Subscribing BEFORE spawning the engine loop
    // guarantees no event is missed between tools-ready and first
    // processed video.
    let mut dl_event_rx = dl_event_tx.subscribe();

    let mut engine_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle API commands (play, pause, skip, etc.)
                Some(cmd) = engine_rx.recv() => {
                    // The full command match lives in `engine_dispatch` (extracted
                    // for the 1000-line cap when #183 D4 added SetDubMix).
                    engine_dispatch::dispatch(&mut engine, cmd).await;
                }
                // Handle pipeline events (started, position, ended, error)
                Some((playlist_id, event)) = engine.recv_pipeline_event() => {
                    engine.handle_pipeline_event(playlist_id, event).await;
                }
                // Handle download worker broadcasts. The message format
                // is `<kind>:<youtube_id>` where kind is `downloading`
                // or `processed`. Only `processed` rewakes pipelines.
                Ok(msg) = dl_event_rx.recv() => {
                    if let Some(youtube_id) = msg.strip_prefix("processed:") {
                        engine.on_video_processed(youtube_id).await;
                    }
                }
                _ = engine_shutdown.recv() => {
                    info!("engine command bridge shutting down");
                    break;
                }
            }
        }
    });

    // 11. Axum HTTP server
    let router = api::router(state, config.dist_dir).merge(peer::router(exchange));
    let listener = {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&std::net::SocketAddr::from(([0, 0, 0, 0], config.port)).into())?;
        socket.listen(128)?;
        tokio::net::TcpListener::from_std(socket.into())?
    };
    info!(port = config.port, "HTTP server listening");

    // Serve with graceful shutdown
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            info!("shutdown signal received, stopping HTTP server");
        })
        .await?;

    // 12. Signal all workers to stop
    let _ = shutdown_tx.send(());
    info!("server stopped");

    Ok(())
}

/// Default interval, in seconds, between periodic playlist re-syncs — used
/// whenever `PLAYLIST_SYNC_INTERVAL_SECS` is absent, unparseable, or zero.
const DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS: u64 = 600;

/// Default interval, in seconds, between periodic yt-dlp self-updates
/// (#140) — used whenever `YTDLP_UPDATE_INTERVAL_SECS` is absent,
/// unparseable, or zero.
const DEFAULT_YTDLP_UPDATE_INTERVAL_SECS: u64 = 86400;

/// Pure parser shared by every periodic-interval env override in this
/// module: falls back to `default` on `None`, on a value that doesn't
/// parse as a `u64`, or on `0` — `tokio::time::interval` panics on a
/// zero-duration period, so zero is treated the same as absent.
fn interval_from(env_value: Option<&str>, default: u64) -> u64 {
    match env_value.and_then(|v| v.parse::<u64>().ok()) {
        Some(secs) if secs > 0 => secs,
        _ => default,
    }
}

/// Pure parser for the periodic playlist re-sync interval override.
/// Falls back to [`DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS`] on `None`, on a
/// value that doesn't parse as a `u64`, or on `0`.
fn sync_interval_from(env_value: Option<&str>) -> u64 {
    interval_from(env_value, DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS)
}

/// Read `PLAYLIST_SYNC_INTERVAL_SECS` from the environment, warning (but
/// still falling back to the default) when the value is present but
/// invalid, so a typo in an operator's env file is visible in the logs
/// instead of silently defaulting.
fn playlist_sync_interval_secs() -> u64 {
    let raw = std::env::var("PLAYLIST_SYNC_INTERVAL_SECS").ok();
    if let Some(v) = &raw {
        let valid = v.parse::<u64>().is_ok_and(|n| n > 0);
        if !valid {
            warn!(
                value = %v,
                default_secs = DEFAULT_PLAYLIST_SYNC_INTERVAL_SECS,
                "PLAYLIST_SYNC_INTERVAL_SECS invalid or zero, using default"
            );
        }
    }
    sync_interval_from(raw.as_deref())
}

/// Periodically re-enqueue a [`SyncRequest`] for every active playlist
/// (#139): the one-shot startup sync only ever fires once, so a video
/// added to a YouTube playlist later would never be picked up otherwise.
/// The first `interval.tick()` resolves immediately — that tick is
/// deliberately consumed and discarded before entering the loop, since the
/// startup sync already covered t=0. Exits on shutdown broadcast.
async fn periodic_playlist_sync(
    pool: SqlitePool,
    sync_tx: mpsc::Sender<SyncRequest>,
    interval_secs: u64,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // immediate first tick — startup sync already covered it
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = interval.tick() => {
                match startup::enqueue_sync_all_active(&pool, &sync_tx).await {
                    Ok(count) => info!(
                        count,
                        "periodic sync: enqueueing one SyncRequest per active playlist"
                    ),
                    Err(e) => warn!("periodic sync enqueue failed: {e}"),
                }
            }
        }
    }
}

/// Read `YTDLP_UPDATE_INTERVAL_SECS` from the environment (#140), warning
/// (but still falling back to the default) when the value is present but
/// invalid — same shape as [`playlist_sync_interval_secs`].
fn ytdlp_update_interval_secs() -> u64 {
    let raw = std::env::var("YTDLP_UPDATE_INTERVAL_SECS").ok();
    if let Some(v) = &raw {
        let valid = v.parse::<u64>().is_ok_and(|n| n > 0);
        if !valid {
            warn!(
                value = %v,
                default_secs = DEFAULT_YTDLP_UPDATE_INTERVAL_SECS,
                "YTDLP_UPDATE_INTERVAL_SECS invalid or zero, using default"
            );
        }
    }
    interval_from(raw.as_deref(), DEFAULT_YTDLP_UPDATE_INTERVAL_SECS)
}

/// Periodically re-run `yt-dlp --update` (#140): the download worker never
/// updates its own yt-dlp binary, so a long-running box falls behind
/// YouTube's format changes over time (see
/// `.claude/rules/youtube-cookies.md`). The startup one-shot update
/// already covers t=0, so the first `interval.tick()` is deliberately
/// consumed and discarded before entering the loop, same pattern as
/// [`periodic_playlist_sync`]. Never fatal — a failed update just leaves
/// the current binary in place. Exits on shutdown broadcast.
async fn periodic_ytdlp_update(
    pool: SqlitePool,
    tools_mgr: downloader::tools::ToolsManager,
    ytdlp_path: PathBuf,
    interval_secs: u64,
    ytdlp_lock: downloader::YtdlpLock,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // immediate first tick — startup update already covered it
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = interval.tick() => {
                // #230: a held background skips this day's update.
                if background_hold::holds(&pool, background_hold::Job::YtdlpUpdate).await {
                    continue;
                }
                // Wait for any in-flight download before touching the binary.
                let _ytdlp_guard = ytdlp_lock.lock().await;
                match tools_mgr.update_ytdlp().await {
                    Ok(()) => {
                        let version = tools_mgr.ytdlp_version(&ytdlp_path).await.ok();
                        info!(version = ?version, "periodic yt-dlp self-update succeeded");
                    }
                    Err(e) => warn!("periodic yt-dlp self-update failed: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod lib_tests;
